use super::{allowed_document, returned_to_provider, start_url, title};
use crate::streams::Provider;
use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle};
use std::{
    cell::{Cell, RefCell},
    num::NonZeroIsize,
    rc::{Rc, Weak},
    sync::OnceLock,
};
use webview2_com::{
    GetCookiesCompletedHandler,
    Microsoft::Web::WebView2::Win32::{
        ICoreWebView2Controller, ICoreWebView2CookieManager, ICoreWebView2Environment,
        ICoreWebView2Profile6, ICoreWebView2Profile8, ICoreWebView2_13, ICoreWebView2_2,
        COREWEBVIEW2_COOKIE_SAME_SITE_KIND,
    },
    NavigationCompletedEventHandler, NavigationStartingEventHandler,
    NewWindowRequestedEventHandler,
};
use windows::core::{Interface, PWSTR};
use windows::Win32::{
    Foundation::{HWND as WinHwnd, LPARAM, LRESULT, WPARAM},
    UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
};
use windows_sys::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST},
    System::LibraryLoader::GetModuleHandleW,
    UI::Input::KeyboardAndMouse::{GetFocus, IsWindowEnabled, SetFocus},
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, GetAncestor, GetClassLongPtrW,
        GetClientRect, GetForegroundWindow, GetWindow, GetWindowRect, IsChild, IsIconic,
        IsWindowVisible, LoadCursorW, RegisterClassExW, SendMessageW, SetForegroundWindow,
        SetWindowTextW, ShowWindow, GA_ROOT, GCLP_HICON, GCLP_HICONSM, GW_OWNER, ICON_BIG,
        ICON_SMALL, IDC_ARROW, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWNOACTIVATE, WM_CLOSE,
        WM_GETICON, WM_NCDESTROY, WM_SETICON, WNDCLASSEXW, WS_CLIPCHILDREN, WS_OVERLAPPEDWINDOW,
        WS_POPUP,
    },
};
use wry::{WebView, WebViewBuilder, WebViewBuilderExtWindows, WebViewExtWindows};

#[path = "windows_cookie_access.rs"]
mod cookie_access;

pub(super) fn watch_requests(
    view: &WebView,
    context: std::rc::Weak<super::Context>,
    ctx: &egui::Context,
) -> Result<(), String> {
    let navigation_context = context.clone();
    let navigation_ctx = ctx.clone();
    let navigation = NavigationStartingEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = args {
            unsafe {
                let mut gesture = windows::core::BOOL::default();
                args.IsUserInitiated(&mut gesture)?;
                let mut destination = PWSTR::null();
                args.Uri(&mut destination)?;
                let destination = webview2_com::take_pwstr(destination);
                let mut auth = windows::core::BOOL::default();
                args.RequestHeaders()?
                    .Contains(windows::core::w!("Authorization"), &mut auth)?;
                if navigation_context.upgrade().is_some_and(|context| {
                    context.request_login(&destination, gesture.as_bool(), auth.as_bool())
                }) {
                    args.SetCancel(true)?;
                    navigation_ctx.request_repaint();
                }
            }
        }
        Ok(())
    }));
    let popup_ctx = ctx.clone();
    let popup = NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = args {
            unsafe {
                let mut gesture = windows::core::BOOL::default();
                args.IsUserInitiated(&mut gesture)?;
                let mut destination = PWSTR::null();
                args.Uri(&mut destination)?;
                let destination = webview2_com::take_pwstr(destination);
                if context.upgrade().is_some_and(|context| {
                    context.request_login(&destination, gesture.as_bool(), false)
                }) {
                    args.SetHandled(true)?;
                    popup_ctx.request_repaint();
                }
            }
        }
        Ok(())
    }));
    let mut registration = 0;
    unsafe {
        let view = view.webview();
        view.add_NavigationStarting(&navigation, &mut registration)
            .and_then(|_| view.add_FrameNavigationStarting(&navigation, &mut registration))
            .and_then(|_| view.add_NewWindowRequested(&popup, &mut registration))
    }
    .map_err(|_| "The stream player could not protect provider sign-in requests.".into())
}

pub(crate) struct Context {
    pub profile_name: String,
    profile_path: std::path::PathBuf,
    pub environment: RefCell<Option<ICoreWebView2Environment>>,
    profile: RefCell<Option<ICoreWebView2Profile8>>,
    keeper: RefCell<Option<Keeper>>,
    controllers: RefCell<Vec<Weak<ICoreWebView2Controller>>>,
    retired: Cell<bool>,
    cookies: RefCell<Option<ICoreWebView2CookieManager>>,
    restore_cookies: RefCell<Vec<super::session::Cookie>>,
}

/// Kept beside its WebView. The context holds only weak registrations, so
/// ordinary POV teardown cannot accumulate controller or browser references.
#[must_use]
pub(crate) struct Registration {
    _controller: Rc<ICoreWebView2Controller>,
}

impl Context {
    pub fn new(provider: &Provider) -> Result<Self, String> {
        Self::for_origin(provider, crate::presence::endpoint_url("/").ok().as_ref())
    }

    fn for_origin(provider: &Provider, origin: Option<&url::Url>) -> Result<Self, String> {
        let profile_name = format!("BrickViewer{}", uuid::Uuid::new_v4().simple());
        let profile_path = cookie_access::prepare(&profile_name, provider, origin)?;
        Ok(Self {
            profile_name,
            profile_path,
            environment: RefCell::new(None),
            profile: RefCell::new(None),
            keeper: RefCell::new(None),
            controllers: RefCell::new(Vec::new()),
            retired: Cell::new(false),
            cookies: RefCell::new(None),
            restore_cookies: RefCell::new(Vec::new()),
        })
    }

    pub fn register(&self, view: &WebView) -> Result<Registration, String> {
        if self.retired.get() {
            return Err("This provider session has ended.".into());
        }
        let profile = unsafe {
            view.webview()
                .cast::<ICoreWebView2_13>()
                .and_then(|view| view.Profile())
        }
        .map_err(|_| "Update WebView2 to use a private provider session.")?;
        let mut private = windows::core::BOOL::default();
        unsafe { profile.IsInPrivateModeEnabled(&mut private) }
            .map_err(|_| "The provider session could not verify private browsing.")?;
        if !private.as_bool() {
            return Err("The provider session requires private browsing.".into());
        }
        let mut name = PWSTR::null();
        unsafe { profile.ProfileName(&mut name) }
            .map_err(|_| "The provider session could not verify its identity.")?;
        if webview2_com::take_pwstr(name) != self.profile_name {
            return Err("The provider session requires its own private profile.".into());
        }
        let mut path = PWSTR::null();
        unsafe { profile.ProfilePath(&mut path) }
            .map_err(|_| "The provider session could not verify its storage.")?;
        let actual = std::fs::canonicalize(webview2_com::take_pwstr(path));
        let expected = std::fs::canonicalize(&self.profile_path);
        if !matches!((actual, expected), (Ok(actual), Ok(expected)) if actual == expected) {
            return Err("The provider session requires its own configured profile.".into());
        }
        let managed = profile
            .cast::<ICoreWebView2Profile8>()
            .map_err(|_| "Update WebView2 to use a private provider session.")?;
        // Do not save passwords/autofill information in the provider session.
        let settings = profile
            .cast::<ICoreWebView2Profile6>()
            .map_err(|_| "The provider session could not protect form data.")?;
        unsafe {
            settings
                .SetIsPasswordAutosaveEnabled(false)
                .and_then(|_| settings.SetIsGeneralAutofillEnabled(false))
        }
        .map_err(|_| "The provider session could not protect form data.")?;
        if self.retired.get() {
            return Err("This provider session has ended.".into());
        }
        let manager = unsafe {
            view.webview()
                .cast::<ICoreWebView2_2>()
                .and_then(|view| view.CookieManager())
        }
        .map_err(|_| "The private viewing session is unavailable.")?;
        let restored = self.restore_cookies.borrow().clone();
        for saved in &restored {
            use windows::core::HSTRING;
            let result = unsafe {
                (|| -> windows::core::Result<()> {
                    let cookie = manager.CreateCookie(
                        &HSTRING::from(&saved.name),
                        &HSTRING::from(&saved.value),
                        &HSTRING::from(&saved.domain),
                        &HSTRING::from(&saved.path),
                    )?;
                    cookie.SetIsSecure(saved.secure)?;
                    cookie.SetIsHttpOnly(saved.http_only)?;
                    cookie.SetSameSite(COREWEBVIEW2_COOKIE_SAME_SITE_KIND(i32::from(
                        saved.same_site,
                    )))?;
                    cookie.SetExpires(saved.expires.map(|at| at as f64).unwrap_or(-1.0))?;
                    manager.AddOrUpdateCookie(&cookie)
                })()
            };
            result.map_err(|_| "The saved viewing login could not be restored.")?;
        }
        self.restore_cookies.borrow_mut().clear();
        *self.cookies.borrow_mut() = Some(manager);
        *self.environment.borrow_mut() = Some(view.environment());
        *self.profile.borrow_mut() = Some(managed);
        let registration = Registration {
            _controller: Rc::new(view.controller()),
        };
        let mut controllers = self.controllers.borrow_mut();
        controllers.retain(|controller| controller.strong_count() != 0);
        controllers.push(Rc::downgrade(&registration._controller));
        Ok(registration)
    }

    fn keep_alive(&self, owner: HWND) -> Result<(), String> {
        if self.keeper.borrow().is_some() {
            return Ok(());
        }
        let class = wide("STATIC");
        let caption = wide("");
        let handle = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                caption.as_ptr(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if handle.is_null() {
            return Err("The private provider session could not start.".into());
        }
        let native = NativeWindow::new(handle, None)?;
        // InPrivate data belongs to live views, not merely a profile handle.
        // Keep one empty, invisible view after explicit sign-in so closing all
        // POVs does not drop this run's session. It never loads any document.
        let mut storage = super::super::windows_profile::context()?;
        let environment = self.environment.borrow().clone();
        let builder = WebViewBuilder::new_with_web_context(&mut storage);
        let builder = match environment {
            Some(environment) => builder.with_environment(environment),
            None => builder,
        };
        let view = builder
            .with_profile_name(&self.profile_name)
            .with_incognito(true)
            .with_visible(false)
            .with_devtools(false)
            .with_clipboard(false)
            .with_navigation_handler(|_| false)
            .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
            .with_download_started_handler(|_, _| false)
            // This can create the environment before any media view exists.
            // Match media's protected defaults, including enabled SmartScreen.
            .with_additional_browser_args(super::super::WINDOWS_BROWSER_ARGS)
            .build(&native)
            .map_err(|_| "The private provider session could not start.")?;
        protect_settings(&view)?;
        super::super::protect_windows_permissions(&view)?;
        let registration = self.register(&view)?;
        *self.keeper.borrow_mut() = Some(Keeper {
            _view: view,
            _registration: registration,
            _native: native,
            _storage: storage,
        });
        Ok(())
    }

    pub fn retire(&self) {
        self.retired.set(true);
        self.cookies.borrow_mut().take();
        self.restore_cookies.borrow_mut().clear();
        let controllers = std::mem::take(&mut *self.controllers.borrow_mut());
        let keeper = self.keeper.borrow_mut().take();
        let profile = self.profile.borrow_mut().take();
        let environment = self.environment.borrow_mut().take();
        // End every live controller synchronously, including media retained by
        // a closing panel. Profile deletion alone may not close InPrivate views
        // promptly. Release RefCell borrows before COM can reenter native code.
        for controller in controllers.into_iter().filter_map(|value| value.upgrade()) {
            let _ = unsafe { controller.Close() };
        }
        drop(keeper);
        if let Some(profile) = profile {
            // Best-effort cleanup of the owned profile directory; it is never
            // reused. Runtime retries pending deletion on future browser starts.
            let _ = unsafe { profile.Delete() };
        }
        drop(environment);
    }

    pub(super) fn restore(&self, cookies: &[super::session::Cookie]) -> Result<(), String> {
        *self.restore_cookies.borrow_mut() = cookies.to_vec();
        Ok(())
    }

    pub(super) fn read_cookies(
        &self,
        provider: &Provider,
        done: impl FnOnce(Result<Vec<super::session::Cookie>, ()>) + 'static,
    ) {
        let manager = self.cookies.borrow().clone();
        let Some(manager) = manager else {
            done(Err(()));
            return;
        };
        let uri = match provider {
            Provider::Youtube => windows::core::w!("https://www.youtube.com/"),
            Provider::Twitch => windows::core::w!("https://www.twitch.tv/"),
        };
        let callback = Rc::new(RefCell::new(Some(done)));
        let completed = callback.clone();
        let handler = GetCookiesCompletedHandler::create(Box::new(move |error, cookies| {
            let result = (|| -> windows::core::Result<Vec<super::session::Cookie>> {
                error?;
                let Some(cookies) = cookies else {
                    return Ok(Vec::new());
                };
                let mut count = 0;
                unsafe {
                    cookies.Count(&mut count)?;
                }
                if count > 256 {
                    return Err(windows::core::Error::from(
                        windows::Win32::Foundation::E_UNEXPECTED,
                    ));
                }
                let mut saved = Vec::new();
                for index in 0..count {
                    unsafe {
                        let cookie = cookies.GetValueAtIndex(index)?;
                        let mut text = PWSTR::null();
                        cookie.Name(&mut text)?;
                        let name = webview2_com::take_pwstr(text);
                        cookie.Value(&mut text)?;
                        let value = webview2_com::take_pwstr(text);
                        cookie.Domain(&mut text)?;
                        let domain = webview2_com::take_pwstr(text);
                        cookie.Path(&mut text)?;
                        let path = webview2_com::take_pwstr(text);
                        let mut expires = 0.0;
                        cookie.Expires(&mut expires)?;
                        let mut session = windows::core::BOOL::default();
                        cookie.IsSession(&mut session)?;
                        let mut secure = windows::core::BOOL::default();
                        cookie.IsSecure(&mut secure)?;
                        let mut http_only = windows::core::BOOL::default();
                        cookie.IsHttpOnly(&mut http_only)?;
                        let mut same_site = COREWEBVIEW2_COOKIE_SAME_SITE_KIND::default();
                        cookie.SameSite(&mut same_site)?;
                        if !expires.is_finite() {
                            return Err(windows::core::Error::from(
                                windows::Win32::Foundation::E_UNEXPECTED,
                            ));
                        }
                        saved.push(super::session::Cookie {
                            name,
                            value,
                            domain,
                            path,
                            expires: (!session.as_bool()).then_some(expires as i64),
                            secure: secure.as_bool(),
                            http_only: http_only.as_bool(),
                            same_site: u8::try_from(same_site.0).unwrap_or(255),
                        });
                    }
                }
                Ok(saved)
            })();
            let done = completed.borrow_mut().take();
            if let Some(done) = done {
                done(result.map_err(|_| ()));
            }
            Ok(())
        }));
        if unsafe { manager.GetCookies(uri, &handler) }.is_err() {
            let done = callback.borrow_mut().take();
            if let Some(done) = done {
                done(Err(()));
            }
        }
    }
}

struct WindowState {
    handle: Cell<HWND>,
    closed: Cell<bool>,
    controller: RefCell<Option<ICoreWebView2Controller>>,
    repaint: Option<egui::Context>,
}

impl WindowState {
    fn release_browser_focus(&self) {
        let handle = self.handle.get();
        if handle.is_null() {
            return;
        }
        // WebView2 152 can leave its thread's hide-while-typing cursor count
        // negative if a focused browser disappears before receiving focus loss.
        // Let the browser unwind that state itself, before hiding/closing it.
        // https://github.com/MicrosoftEdge/WebView2Feedback/issues/5687
        unsafe {
            let focus = GetFocus();
            if GetForegroundWindow() != handle
                || focus.is_null()
                || (focus != handle && IsChild(handle, focus) == 0)
            {
                return;
            }
            let owner = GetWindow(handle, GW_OWNER);
            if !owner.is_null() && IsWindowVisible(owner) != 0 && IsWindowEnabled(owner) != 0 {
                SetFocus(owner);
            } else {
                // Do not activate a hidden/disabled owner or another app.
                SetFocus(std::ptr::null_mut());
            }
        }
    }

    fn hide(&self) {
        self.release_browser_focus();
        // Focus callbacks can reenter teardown, so reread the guarded handle.
        let handle = self.handle.get();
        if !handle.is_null() {
            unsafe { ShowWindow(handle, SW_HIDE) };
        }
    }
}

struct NativeWindow(Rc<WindowState>);

fn provider_window_class() -> Result<u16, String> {
    static CLASS: OnceLock<Result<u16, String>> = OnceLock::new();
    CLASS
        .get_or_init(|| {
            let name = wide("Brick.ProviderSignIn");
            // STATIC is a control class: its WM_NCHITTEST returns
            // HTTRANSPARENT, including over the caption. Use the normal
            // window procedure for dragging, resizing and the system menu.
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(DefWindowProcW),
                hInstance: unsafe { GetModuleHandleW(std::ptr::null()) },
                hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) },
                lpszClassName: name.as_ptr(),
                ..Default::default()
            };
            let atom = unsafe { RegisterClassExW(&class) };
            if atom == 0 {
                Err("The provider sign-in window could not register its controls.".into())
            } else {
                Ok(atom)
            }
        })
        .clone()
}

fn inherit_window_icons(handle: HWND, owner: HWND) {
    // These icons belong to Brick's owner window, which outlives its owned
    // popups. Borrow them; never destroy or replace the owner's icon handles.
    for (size, class_icon) in [(ICON_SMALL, GCLP_HICONSM), (ICON_BIG, GCLP_HICON)] {
        let mut icon = unsafe { SendMessageW(owner, WM_GETICON, size as usize, 0) };
        if icon == 0 {
            icon = unsafe { GetClassLongPtrW(owner, class_icon) } as isize;
        }
        if icon != 0 {
            unsafe { SendMessageW(handle, WM_SETICON, size as usize, icon) };
        }
    }
}

impl NativeWindow {
    fn new(handle: HWND, repaint: Option<&egui::Context>) -> Result<Self, String> {
        let state = Rc::new(WindowState {
            handle: Cell::new(handle),
            closed: Cell::new(false),
            controller: RefCell::new(None),
            repaint: repaint.cloned(),
        });
        let callback = Rc::into_raw(Rc::clone(&state)) as usize;
        if !unsafe { SetWindowSubclass(WinHwnd(handle), Some(window_lifetime), 1, callback) }
            .as_bool()
        {
            // Installation failed, so Windows never owns this callback ref.
            unsafe {
                drop(Rc::from_raw(callback as *const WindowState));
                DestroyWindow(handle);
            }
            return Err("The provider window could not protect its lifetime.".into());
        }
        Ok(Self(state))
    }

    fn handle(&self) -> HWND {
        self.0.handle.get()
    }
}

unsafe extern "system" fn window_lifetime(
    hwnd: WinHwnd,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    // One Rc belongs to this subclass until WM_NCDESTROY. No callback points
    // into a movable Window struct, and no stale HWND establishes ownership.
    let pointer = data as *const WindowState;
    // COM calls can pump nested native messages. Keep this callback's state
    // alive even if nested teardown releases both the window and subclass.
    unsafe { Rc::increment_strong_count(pointer) };
    let state = unsafe { Rc::from_raw(pointer) };
    if message == WM_CLOSE {
        state.closed.set(true);
        state.hide();
        let controller = state.controller.borrow_mut().take();
        if let Some(controller) = controller {
            let _ = unsafe { controller.Close() };
        }
        if let Some(ctx) = &state.repaint {
            ctx.request_repaint();
        }
        return LRESULT(0);
    }
    if message == WM_NCDESTROY {
        state.handle.set(std::ptr::null_mut());
        state.closed.set(true);
        unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(window_lifetime), id);
            drop(Rc::from_raw(data as *const WindowState));
        }
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

struct Keeper {
    _view: WebView,
    _registration: Registration,
    _native: NativeWindow,
    _storage: wry::WebContext,
}
impl HasWindowHandle for NativeWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, raw_window_handle::HandleError> {
        let hwnd = NonZeroIsize::new(self.handle() as isize)
            .ok_or(raw_window_handle::HandleError::Unavailable)?;
        // This guard owns the HWND and outlives the child WebView.
        Ok(unsafe {
            WindowHandle::borrow_raw(RawWindowHandle::Win32(Win32WindowHandle::new(hwnd)))
        })
    }
}
impl Drop for NativeWindow {
    fn drop(&mut self) {
        let handle = self.handle();
        if !handle.is_null() {
            unsafe {
                DestroyWindow(handle);
            }
        }
    }
}

pub(super) struct Window {
    pub completion: Rc<super::LoginCompletion>,
    // Drop the WebView before its native parent.
    view: WebView,
    _registration: Registration,
    native: NativeWindow,
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn centered_popup(owner: RECT, work: RECT, width: i32, height: i32) -> Option<RECT> {
    // Screen coordinates can be negative on secondary monitors. Calculate in
    // i64 so centering an off-screen owner cannot overflow before clamping.
    let left = i64::from(work.left);
    let top = i64::from(work.top);
    let right = i64::from(work.right);
    let bottom = i64::from(work.bottom);
    if right <= left || bottom <= top {
        return None;
    }
    let width = i64::from(width).max(1).min(right - left);
    let height = i64::from(height).max(1).min(bottom - top);
    let x =
        ((i64::from(owner.left) + i64::from(owner.right) - width) / 2).clamp(left, right - width);
    let y =
        ((i64::from(owner.top) + i64::from(owner.bottom) - height) / 2).clamp(top, bottom - height);
    Some(RECT {
        left: x as i32,
        top: y as i32,
        right: (x + width) as i32,
        bottom: (y + height) as i32,
    })
}

impl Window {
    pub fn from_frame(
        context: &Context,
        provider: &Provider,
        frame: &eframe::Frame,
        ctx: &egui::Context,
    ) -> Result<Self, String> {
        let handle = frame
            .window_handle()
            .map_err(|_| "The provider sign-in window could not find Brick.")?;
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return Err("Provider sign-in needs Brick's Windows desktop window.".into());
        };
        Self::new_at_owner(
            context,
            provider,
            handle.hwnd.get() as HWND,
            ctx,
            start_url(provider),
            allowed_document,
            returned_to_provider,
        )
    }

    pub fn new(
        context: &Context,
        provider: &Provider,
        player: &WebView,
        ctx: &egui::Context,
    ) -> Result<Self, String> {
        Self::new_at(
            context,
            provider,
            player,
            ctx,
            start_url(provider),
            allowed_document,
            returned_to_provider,
        )
    }

    fn new_at(
        context: &Context,
        provider: &Provider,
        player: &WebView,
        ctx: &egui::Context,
        start: &str,
        permits: fn(&Provider, &str) -> bool,
        returned: fn(&Provider, &str) -> bool,
    ) -> Result<Self, String> {
        let mut owner = windows::Win32::Foundation::HWND::default();
        unsafe { player.controller().ParentWindow(&mut owner) }
            .map_err(|_| "The provider sign-in window could not find Brick.")?;
        Self::new_at_owner(context, provider, owner.0, ctx, start, permits, returned)
    }

    fn new_at_owner(
        context: &Context,
        provider: &Provider,
        owner: HWND,
        ctx: &egui::Context,
        start: &str,
        permits: fn(&Provider, &str) -> bool,
        returned: fn(&Provider, &str) -> bool,
    ) -> Result<Self, String> {
        // This is borrowed from the live Frame or media controller. Only the
        // newly created popup/keeper HWNDs are owned and destroyed by us.
        let owner = unsafe { GetAncestor(owner, GA_ROOT) };
        if owner.is_null() {
            return Err("The provider sign-in window could not find Brick.".into());
        }
        context.keep_alive(owner)?;
        let environment = context
            .environment
            .borrow()
            .clone()
            .ok_or("The private provider session could not start.")?;
        let mut monitor = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let mut owner_rect = RECT::default();
        if unsafe {
            GetMonitorInfoW(
                MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST),
                &mut monitor,
            )
        } == 0
            || unsafe { GetWindowRect(owner, &mut owner_rect) } == 0
        {
            return Err("The provider sign-in window could not find its display.".into());
        }
        let scale = ctx
            .native_pixels_per_point()
            .filter(|scale| scale.is_finite() && *scale > 0.0)
            .unwrap_or(1.0);
        let bounds = centered_popup(
            owner_rect,
            monitor.rcWork,
            (520.0 * scale).round() as i32,
            (640.0 * scale).round() as i32,
        )
        .ok_or("The provider sign-in window could not fit its display.")?;
        let caption = wide(&title(provider, start_url(provider)));
        let class = provider_window_class()?;
        // Wry follows the native window's size and position. The lifetime
        // subclass keeps HWND ownership valid when closing this controller.
        let handle = unsafe {
            CreateWindowExW(
                0,
                class as usize as *const u16,
                caption.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
                bounds.left,
                bounds.top,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                owner,
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            )
        };
        if handle.is_null() {
            return Err("The provider sign-in window could not open.".into());
        }
        let native = NativeWindow::new(handle, Some(ctx))?;
        inherit_window_icons(handle, owner);
        let mut rect = windows_sys::Win32::Foundation::RECT::default();
        if unsafe { GetClientRect(handle, &mut rect) } == 0 {
            return Err("The provider sign-in window could not size its browser.".into());
        }
        let nav_provider = provider.clone();
        let completion = Rc::new(super::LoginCompletion::default());
        let navigation_completion = completion.clone();
        let navigation_window = Rc::downgrade(&native.0);
        let title_provider = provider.clone();
        let title_window = Rc::downgrade(&native.0);
        let view = WebViewBuilder::new()
            .with_environment(environment)
            .with_profile_name(&context.profile_name)
            .with_background_color((21, 24, 29, 255))
            .with_incognito(true)
            .with_devtools(false)
            .with_clipboard(false)
            .with_hotkeys_zoom(false)
            .with_bounds(wry::Rect {
                position: wry::dpi::PhysicalPosition::new(0, 0).into(),
                size: wry::dpi::PhysicalSize::new(
                    rect.right.max(1) as u32,
                    rect.bottom.max(1) as u32,
                )
                .into(),
            })
            .with_navigation_handler(move |url| {
                let allowed = permits(&nav_provider, &url);
                if allowed {
                    // Wry's navigation handler is top-level NavigationStarting,
                    // before ContentLoading/paint. Frame policy is separate.
                    navigation_completion.started();
                    let home = returned(&nav_provider, &url);
                    let reveal = if home {
                        navigation_completion.hide_return();
                        false
                    } else {
                        navigation_completion.reveal()
                    };
                    if let Some(window) = navigation_window.upgrade().filter(|w| !w.closed.get()) {
                        unsafe {
                            if home {
                                window.hide();
                            } else if reveal {
                                ShowWindow(window.handle.get(), SW_SHOWNOACTIVATE);
                            }
                        }
                    }
                }
                allowed
            })
            .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
            .with_download_started_handler(|_, _| false)
            .with_on_page_load_handler(move |_, url| {
                if let Some(window) = title_window
                    .upgrade()
                    .filter(|window| !window.handle.get().is_null() && !window.closed.get())
                {
                    let text = wide(&title(&title_provider, &url));
                    unsafe {
                        SetWindowTextW(window.handle.get(), text.as_ptr());
                    }
                }
            })
            .build(&native)
            .map_err(|_| "The provider sign-in browser could not start.")?;
        // This is deliberately not StreamPlayer: no bearer request, injected
        // scripts, native messaging, preferences, capture or host objects.
        protect_settings(&view)?;
        super::super::protect_windows_permissions(&view)?;
        // NavigationCompleted can arrive for a cancelled older document after
        // the next navigation has started. It must not reveal or finish that
        // newer return, even if Source already reports the new page's URL.
        let navigation_id = Rc::new(Cell::new(None));
        let started_id = navigation_id.clone();
        let started = NavigationStartingEventHandler::create(Box::new(move |_, args| {
            if let Some(args) = args {
                let mut id = 0;
                unsafe { args.NavigationId(&mut id)? };
                started_id.set(Some(id));
            }
            Ok(())
        }));
        let mut started_token = 0;
        unsafe {
            view.webview()
                .add_NavigationStarting(&started, &mut started_token)
        }
        .map_err(|_| "The provider sign-in browser could not watch navigation.")?;
        let return_provider = provider.clone();
        let return_completion = completion.clone();
        let return_window = Rc::downgrade(&native.0);
        let finished = ctx.clone();
        let completed = NavigationCompletedEventHandler::create(Box::new(move |view, args| {
            let (Some(view), Some(args)) = (view, args) else {
                return Ok(());
            };
            unsafe {
                let mut id = 0;
                args.NavigationId(&mut id)?;
                if navigation_id.get() != Some(id) {
                    return Ok(());
                }
                let mut success = windows::core::BOOL::default();
                args.IsSuccess(&mut success)?;
                if !success.as_bool() {
                    if return_completion.reveal() {
                        if let Some(window) = return_window.upgrade().filter(|w| !w.closed.get()) {
                            ShowWindow(window.handle.get(), SW_SHOWNOACTIVATE);
                        }
                    }
                    return Ok(());
                }
                let mut source = PWSTR::null();
                view.Source(&mut source)?;
                return_completion.finished(returned(
                    &return_provider,
                    &webview2_com::take_pwstr(source),
                ));
                finished.request_repaint();
            }
            Ok(())
        }));
        let mut completion_token = 0;
        unsafe {
            view.webview()
                .add_NavigationCompleted(&completed, &mut completion_token)
        }
        .map_err(|_| "The provider sign-in browser could not watch its return.".to_owned())?;
        let frame_provider = provider.clone();
        let handler = NavigationStartingEventHandler::create(Box::new(move |_, args| {
            if let Some(args) = args {
                unsafe {
                    args.SetCancel(true)?;
                    let mut uri = PWSTR::null();
                    args.Uri(&mut uri)?;
                    let uri = webview2_com::take_pwstr(uri);
                    if permits(&frame_provider, &uri) {
                        args.SetCancel(false)?;
                    }
                }
            }
            Ok(())
        }));
        let mut registration = 0;
        unsafe {
            view.webview()
                .add_FrameNavigationStarting(&handler, &mut registration)
        }
        .map_err(|_| "The provider sign-in browser could not protect navigation.")?;
        let registration = context.register(&view)?;
        *native.0.controller.borrow_mut() = Some(view.controller());
        view.load_url(start)
            .map_err(|_| "The provider sign-in page could not open.")?;
        unsafe {
            ShowWindow(handle, SW_SHOW);
            SetForegroundWindow(handle);
        }
        Ok(Self {
            completion,
            view,
            _registration: registration,
            native,
        })
    }

    pub fn open(&self) -> bool {
        !self.native.0.closed.get() && !self.native.handle().is_null()
    }
    pub fn reveal(&self) {
        if self.completion.reveal() && self.open() {
            unsafe { ShowWindow(self.native.handle(), SW_SHOWNOACTIVATE) };
        }
    }
    pub fn present(&self) {
        self.reveal();
        unsafe {
            if IsIconic(self.native.handle()) != 0 {
                ShowWindow(self.native.handle(), SW_RESTORE);
            }
            SetForegroundWindow(self.native.handle());
        }
    }
}

fn protect_settings(view: &WebView) -> Result<(), String> {
    unsafe {
        let settings = view
            .webview()
            .Settings()
            .map_err(|_| "The provider sign-in browser could not be protected.")?;
        settings
            .SetIsWebMessageEnabled(false)
            .and_then(|_| settings.SetAreHostObjectsAllowed(false))
            .and_then(|_| settings.SetAreDefaultContextMenusEnabled(false))
            .map_err(|_| "The provider sign-in browser could not be protected.")?;
    }
    Ok(())
}

impl Drop for Window {
    fn drop(&mut self) {
        self.native.0.release_browser_focus();
        let _ = self.view.set_visible(false);
        let _ = unsafe { self.view.webview().Stop() };
    }
}

#[cfg(test)]
mod tests {
    use super::super::ProviderSessions;
    use super::*;
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread,
        time::{Duration, Instant},
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, IsWindow, PeekMessageW, SendMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    #[test]
    fn popup_centers_on_its_owner_on_a_negative_coordinate_monitor() {
        let owner = RECT {
            left: -1800,
            top: 100,
            right: -1200,
            bottom: 500,
        };
        let work = RECT {
            left: -1920,
            top: 40,
            right: 0,
            bottom: 1080,
        };
        let result = centered_popup(owner, work, 400, 300).unwrap();
        assert_eq!((result.left, result.top), (-1700, 150));
        assert_eq!((result.right, result.bottom), (-1300, 450));
    }

    #[test]
    fn popup_fits_small_work_area_even_when_owner_is_far_off_screen() {
        let owner = RECT {
            left: i32::MAX - 1000,
            top: i32::MIN,
            right: i32::MAX,
            bottom: i32::MIN + 800,
        };
        let work = RECT {
            left: -800,
            top: -500,
            right: -100,
            bottom: -100,
        };
        let result = centered_popup(owner, work, 1000, 800).unwrap();
        assert_eq!((result.left, result.top), (work.left, work.top));
        assert_eq!((result.right, result.bottom), (work.right, work.bottom));
        assert!(centered_popup(owner, RECT::default(), 1000, 800).is_none());
    }

    fn pump() {
        let mut message = MSG::default();
        unsafe {
            while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    #[track_caller]
    fn wait_for(stage: &str, mut condition: impl FnMut() -> bool) {
        eprintln!("Native provider fixture: {stage}");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "Native provider fixture timed out waiting for {stage}"
            );
            pump();
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn fixture_origin(_: &Provider, value: &str) -> bool {
        url::Url::parse(value).is_ok_and(|url| {
            url.scheme() == "http"
                && url.host_str() == Some("127.0.0.1")
                && url.username().is_empty()
                && url.password().is_none()
        })
    }

    fn parent_window() -> NativeWindow {
        let class = wide("STATIC");
        let caption = wide("Brick synthetic provider fixture");
        let handle = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                caption.as_ptr(),
                WS_POPUP,
                0,
                0,
                640,
                480,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(!handle.is_null());
        // Shared synthetic icon: exercise inheritance without the real app
        // window, any account, or a desktop asset outside this fixture.
        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::{LoadIconW, IDI_APPLICATION};
            let icon = LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
            assert!(!icon.is_null());
            for size in [ICON_SMALL, ICON_BIG] {
                SendMessageW(handle, WM_SETICON, size as usize, icon as isize);
            }
        }
        NativeWindow::new(handle, None).unwrap()
    }

    fn assert_window_controls(login: &Window, owner: HWND) {
        use windows_sys::Win32::{
            Foundation::POINT,
            Graphics::Gdi::ClientToScreen,
            UI::WindowsAndMessaging::{MoveWindow, HTCAPTION, SW_MINIMIZE, WM_NCHITTEST},
        };
        let handle = login.native.handle();
        for size in [ICON_SMALL, ICON_BIG] {
            let icon = unsafe { SendMessageW(handle, WM_GETICON, size as usize, 0) };
            assert_ne!(icon, 0, "provider window has an icon");
            assert_eq!(icon, unsafe {
                SendMessageW(owner, WM_GETICON, size as usize, 0)
            });
        }
        let mut before = RECT::default();
        let mut client_origin = POINT::default();
        unsafe {
            assert_ne!(GetWindowRect(handle, &mut before), 0);
            assert_ne!(ClientToScreen(handle, &mut client_origin), 0);
        }
        let caption_x = (before.left + before.right) / 2;
        let caption_y = (before.top + client_origin.y) / 2;
        let point = (caption_x as u16 as u32 | ((caption_y as u16 as u32) << 16)) as isize;
        assert_eq!(
            unsafe { SendMessageW(handle, WM_NCHITTEST, 0, point) },
            HTCAPTION as isize,
            "the native title bar must accept dragging, not pass through to Brick"
        );
        unsafe {
            assert_ne!(
                MoveWindow(
                    handle,
                    before.left + 32,
                    before.top + 24,
                    before.right - before.left + 80,
                    before.bottom - before.top + 60,
                    1
                ),
                0
            );
        }
        let mut after = RECT::default();
        assert_ne!(unsafe { GetWindowRect(handle, &mut after) }, 0);
        assert_eq!((after.left, after.top), (before.left + 32, before.top + 24));
        wait_for("resized sign-in browser follows its window", || {
            let mut client = RECT::default();
            let mut browser = windows::Win32::Foundation::RECT::default();
            unsafe {
                GetClientRect(handle, &mut client) != 0
                    && login.view.controller().Bounds(&mut browser).is_ok()
                    && browser.left == 0
                    && browser.top == 0
                    && browser.right == client.right
                    && browser.bottom == client.bottom
            }
        });
        unsafe { ShowWindow(handle, SW_MINIMIZE) };
        assert_ne!(unsafe { IsIconic(handle) }, 0);
        login.present();
        assert_eq!(
            unsafe { IsIconic(handle) },
            0,
            "reopening restores a minimized sign-in window"
        );
    }

    struct MediaView {
        view: WebView,
        _registration: Registration,
    }

    impl std::ops::Deref for MediaView {
        type Target = WebView;
        fn deref(&self) -> &Self::Target {
            &self.view
        }
    }

    fn media_view(context: &Context, parent: &NativeWindow, root: &std::path::Path) -> MediaView {
        // Every provider/account uses production's one app-owned UDF. Isolation
        // must come from distinct InPrivate profiles, never fixture directories.
        assert!(super::super::super::windows_profile::data_directory()
            .unwrap()
            .starts_with(root));
        let mut storage = super::super::super::windows_profile::context().unwrap();
        let builder = WebViewBuilder::new_with_web_context(&mut storage)
            .with_profile_name(&context.profile_name)
            .with_incognito(true)
            .with_additional_browser_args(super::super::super::WINDOWS_BROWSER_ARGS);
        let builder = match context.environment.borrow().as_ref() {
            Some(environment) => builder.with_environment(environment.clone()),
            None => builder,
        };
        let view = builder.build_as_child(parent).unwrap();
        let registration = context.register(&view).unwrap();
        MediaView {
            view,
            _registration: registration,
        }
    }

    fn page_title(view: &WebView) -> Option<String> {
        let mut title = PWSTR::null();
        unsafe { view.webview().DocumentTitle(&mut title) }.ok()?;
        Some(webview2_com::take_pwstr(title))
    }

    fn fixture_request(socket: &mut TcpStream) -> std::io::Result<String> {
        // Winsock accepts can inherit the listener's nonblocking mode. A
        // browser request may also arrive over multiple reads; do not respond
        // and close its connection before the complete bounded header arrives.
        socket.set_nonblocking(false)?;
        socket.set_write_timeout(Some(Duration::from_secs(2)))?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut request = Vec::new();
        let mut chunk = [0; 1024];
        while request.len() < 8192 {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "Fixture request timed out")
                })?;
            socket.set_read_timeout(Some(remaining))?;
            let available = chunk.len().min(8192 - request.len());
            let count = socket.read(&mut chunk[..available])?;
            if count == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            request.extend_from_slice(&chunk[..count]);
            if request.windows(4).any(|part| part == b"\r\n\r\n") {
                return Ok(String::from_utf8_lossy(&request).into_owned());
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Fixture request header exceeded its limit",
        ))
    }

    #[track_caller]
    fn state(view: &WebView, stage: &str) -> serde_json::Value {
        struct Observe<'a>(&'a WebView);
        impl Drop for Observe<'_> {
            fn drop(&mut self) {
                if std::thread::panicking() {
                    let mut source = PWSTR::null();
                    let source = unsafe { self.0.webview().Source(&mut source) }
                        .map(|()| webview2_com::take_pwstr(source));
                    eprintln!(
                        "Fixture page source: {source:?}; title: {:?}",
                        page_title(self.0)
                    );
                }
            }
        }
        let _observe = Observe(view);
        wait_for(stage, || {
            page_title(view).is_some_and(|value| value.starts_with('{'))
        });
        serde_json::from_str(&page_title(view).unwrap()).unwrap()
    }

    #[test]
    #[ignore = "requires an isolated Windows desktop, disposable LOCALAPPDATA and loopback-only network"]
    fn native_windows_provider_session_lifecycle() {
        assert_eq!(std::env::var("BRICK_PROVIDER_FIXTURE").as_deref(), Ok("1"));
        let root = PathBuf::from(std::env::var_os("BRICK_PROVIDER_FIXTURE_DIR").unwrap());
        assert!(root.is_absolute());
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("brick-provider-fixture-"));
        let local = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap());
        assert!(local.starts_with(&root));
        std::fs::create_dir_all(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let stopped = Arc::new(AtomicBool::new(false));
        let stop_server = Arc::clone(&stopped);
        let worker = thread::spawn(move || {
            while !stop_server.load(Ordering::Relaxed) {
                let Ok((mut socket, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let Ok(request) = fixture_request(&mut socket) else {
                    continue;
                };
                let login = request.starts_with("GET /login ");
                let cookie = request.lines().any(|line| {
                    line.to_ascii_lowercase().starts_with("cookie:")
                        && line.contains("brick_fixture_session=fixture-only")
                });
                let auth = request
                    .lines()
                    .any(|line| line.to_ascii_lowercase().starts_with("authorization:"));
                let set = if login {
                    "Set-Cookie: brick_fixture_session=fixture-only; HttpOnly; SameSite=Strict; Path=/\r\n"
                } else {
                    ""
                };
                let script = if login {
                    "localStorage.setItem('fixture-login','fixture-only');"
                } else {
                    ""
                };
                let body = format!("<!doctype html><script>{script}document.title=JSON.stringify({{cookie:{cookie},auth:{auth},storage:localStorage.getItem('fixture-login')==='fixture-only',ipc:typeof window.ipc!=='undefined',bridge:typeof window.brickMedia!=='undefined',opener:window.opener!==null}});</script>");
                let body = if request.starts_with("GET /cursor ") {
                    "<!doctype html><input autofocus style='margin:20px'><script>document.title='cursor-ready';document.querySelector('input').oninput=()=>document.title='cursor-typed';</script>".to_owned()
                } else {
                    body
                };
                let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nCache-Control: no-store\r\n{set}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        struct Stop(Arc<AtomicBool>);
        impl Drop for Stop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let _stop = Stop(Arc::clone(&stopped));
        let parent = parent_window();
        let restored = Context::new(&Provider::Youtube).unwrap();
        let saved = ["SID", "HSID"].map(|name| super::super::session::Cookie {
            name: name.into(),
            value: "synthetic-provider-session".into(),
            domain: ".youtube.com".into(),
            path: "/".into(),
            expires: Some(super::super::session::now() + 3600),
            secure: true,
            http_only: true,
            same_site: 1,
        });
        restored.restore(&saved).unwrap();
        let restored_view = media_view(&restored, &parent, &root);
        let read = Rc::new(RefCell::new(None));
        let completed = read.clone();
        restored.read_cookies(&Provider::Youtube, move |result| {
            *completed.borrow_mut() = Some(result)
        });
        wait_for("protected cookies restored into InPrivate", || {
            read.borrow().is_some()
        });
        let cookies = read.borrow_mut().take().unwrap().unwrap();
        assert!(super::super::session::signed_in(
            &Provider::Youtube,
            &cookies,
            super::super::session::now()
        ));
        assert!(cookies
            .iter()
            .all(|cookie| cookie.secure && cookie.http_only && cookie.same_site == 1));
        assert!(!super::super::session::signed_in(
            &Provider::Twitch,
            &cookies,
            super::super::session::now()
        ));
        drop(restored_view);
        restored.retire();
        drop(restored);
        let sessions = ProviderSessions::default();
        let youtube = sessions.context(Provider::Youtube).unwrap();
        assert!(youtube.platform.environment.borrow().is_none());
        assert!(youtube.platform.keeper.borrow().is_none());
        // Home-first: bootstrap from Brick's borrowed parent HWND, without a
        // media view, bearer request, authenticated wrapper or real provider.
        let login = Window::new_at_owner(
            &youtube.platform,
            &Provider::Youtube,
            parent.handle(),
            &egui::Context::default(),
            &format!("{origin}/login"),
            fixture_origin,
            |_, value| value.ends_with("/returned"),
        )
        .unwrap();
        let initial = state(&login.view, "initial sign-in page");
        login.present();
        wait_for(
            "initial sign-in foreground",
            || unsafe { GetForegroundWindow() } == login.native.handle(),
        );
        assert_window_controls(&login, parent.handle());
        for key in ["auth", "ipc", "bridge", "opener"] {
            assert_eq!(initial[key], false, "{key}");
        }
        let login_handle = login.native.handle();
        let login_lifetime = Rc::downgrade(&login.native.0);
        let old_login = login.view.webview();
        *youtube.window.borrow_mut() = Some(login);
        youtube.attempted.set(true);
        assert!(sessions.login_open());
        assert!(sessions.session_started(&Provider::Youtube));
        assert!(sessions.login_open_for(&Provider::Youtube));
        youtube
            .open_with(|| panic!("An open login must reuse its existing native window"))
            .unwrap();
        let first_pov = media_view(&youtube.platform, &parent, &root);
        first_pov.load_url(&format!("{origin}/state")).unwrap();
        let first_media_state = state(&first_pov, "first media after Home sign-in");
        assert_eq!(first_media_state["cookie"], true);
        assert_eq!(first_media_state["storage"], true);
        unsafe { SendMessageW(login_handle, WM_CLOSE, 0, 0) };
        // The controller closes immediately; the still-owned hidden HWND is
        // destroyed by the next UI inspection, never through a stale handle.
        assert_ne!(unsafe { IsWindow(login_handle) }, 0);
        let mut closed_title = PWSTR::null();
        assert!(unsafe { old_login.DocumentTitle(&mut closed_title) }.is_err());
        assert!(!sessions.login_open());
        assert!(login_lifetime.upgrade().is_none());
        drop(first_pov);
        pump();
        {
            let keeper = youtube.platform.keeper.borrow();
            let mut source = PWSTR::null();
            unsafe { keeper.as_ref().unwrap()._view.webview().Source(&mut source) }.unwrap();
            assert!(matches!(
                webview2_com::take_pwstr(source).as_str(),
                "" | "about:blank"
            ));
        }
        let snapshot = |context: &super::super::Context| {
            let view = media_view(&context.platform, &parent, &root);
            view.load_url(&format!("{origin}/state")).unwrap();
            let result = state(&view, "provider/account snapshot page");
            drop(view);
            pump();
            result
        };
        // No visible login or POV remains; only the empty keeper carries state.
        assert_eq!(snapshot(&youtube)["cookie"], true);
        assert_eq!(snapshot(&youtube)["storage"], true);
        let returned_login = Window::new_at_owner(
            &youtube.platform,
            &Provider::Youtube,
            parent.handle(),
            &egui::Context::default(),
            &format!("{origin}/login"),
            fixture_origin,
            |_, value| value.ends_with("/returned"),
        )
        .unwrap();
        assert_eq!(
            state(&returned_login.view, "login before provider return")["cookie"],
            true
        );
        returned_login
            .view
            .load_url(&format!("{origin}/returned"))
            .unwrap();
        wait_for("provider return stays alive while hidden", || {
            returned_login.completion.returned.get()
        });
        assert_eq!(
            unsafe { IsWindowVisible(returned_login.native.handle()) },
            0
        );
        assert!(returned_login.open());
        *youtube.window.borrow_mut() = Some(returned_login);
        assert!(sessions.login_open());
        youtube.close_window();
        assert!(!sessions.login_open());
        assert!(sessions.session_started(&Provider::Youtube));
        assert_eq!(snapshot(&youtube)["cookie"], true);
        let twitch = sessions.context(Provider::Twitch).unwrap();
        assert_ne!(youtube.platform.profile_name, twitch.platform.profile_name);
        assert_eq!(snapshot(&twitch)["cookie"], false);
        let other_account = ProviderSessions::default();
        let other_youtube = other_account.context(Provider::Youtube).unwrap();
        assert_eq!(snapshot(&other_youtube)["cookie"], false);
        let active_pov = media_view(&youtube.platform, &parent, &root);
        active_pov.load_url(&format!("{origin}/state")).unwrap();
        assert_eq!(
            state(&active_pov, "active media before disconnect")["cookie"],
            true
        );
        // Controller.Close releases its event handlers synchronously. Observe
        // that teardown from inside the released handler, where native reentry
        // must see a retired context with none of its RefCells still borrowed.
        struct ObserveRetirement {
            context: Weak<super::super::Context>,
            observed: Rc<Cell<Option<bool>>>,
        }
        impl Drop for ObserveRetirement {
            fn drop(&mut self) {
                let ready = self.context.upgrade().is_some_and(|context| {
                    let context = &context.platform;
                    context.retired.get()
                        && context.controllers.try_borrow_mut().is_ok()
                        && context.keeper.try_borrow_mut().is_ok()
                        && context.profile.try_borrow_mut().is_ok()
                        && context.environment.try_borrow_mut().is_ok()
                });
                self.observed.set(Some(ready));
            }
        }
        let retirement_observed = Rc::new(Cell::new(None));
        let observation = ObserveRetirement {
            context: Rc::downgrade(&youtube),
            observed: Rc::clone(&retirement_observed),
        };
        let observer = NavigationStartingEventHandler::create(Box::new(move |_, _| {
            let _ = &observation;
            Ok(())
        }));
        let mut event_token = 0;
        unsafe {
            active_pov
                .webview()
                .add_NavigationStarting(&observer, &mut event_token)
        }
        .unwrap();
        drop(observer);
        let reopened = Window::new_at(
            &youtube.platform,
            &Provider::Youtube,
            &active_pov,
            &egui::Context::default(),
            &format!("{origin}/state"),
            fixture_origin,
            |_, value| value.ends_with("/returned"),
        )
        .unwrap();
        assert_eq!(
            state(&reopened.view, "reopened sign-in page")["cookie"],
            true
        );
        reopened.present();
        wait_for(
            "reopened sign-in foreground",
            || unsafe { GetForegroundWindow() } == reopened.native.handle(),
        );
        *youtube.window.borrow_mut() = Some(reopened);
        sessions.disconnect(&Provider::Youtube);
        assert!(!youtube.active());
        assert!(!sessions.login_open());
        assert!(youtube.platform.keeper.borrow().is_none());
        assert_eq!(retirement_observed.get(), Some(true));
        // Retirement must end a retained media controller immediately, without
        // waiting for profile cleanup. Check actual navigation/script rejection,
        // not only a potentially cached Source getter on the old COM object.
        assert!(active_pov.load_url(&format!("{origin}/state")).is_err());
        let completed = webview2_com::ExecuteScriptCompletedHandler::create(Box::new(|_, _| {
            panic!("A retired provider view must not execute scripts")
        }));
        assert!(unsafe {
            active_pov.webview().ExecuteScript(
                windows::core::w!("document.title = 'retired-script-ran'"),
                &completed,
            )
        }
        .is_err());
        assert!(youtube.platform.controllers.borrow().is_empty());
        assert!(youtube.platform.register(&active_pov).is_err());
        assert!(twitch.active());
        assert_eq!(snapshot(&twitch)["cookie"], false);
        let replacement = sessions.context(Provider::Youtube).unwrap();
        assert_ne!(
            replacement.platform.profile_name,
            youtube.platform.profile_name
        );
        assert_eq!(snapshot(&replacement)["cookie"], false);
        assert_eq!(snapshot(&replacement)["storage"], false);
        // Churning POVs drops their strong registration immediately and prunes
        // dead weak entries on the next registration, even with a keeper alive.
        for _ in 0..8 {
            let view = media_view(&replacement.platform, &parent, &root);
            assert_eq!(replacement.platform.controllers.borrow().len(), 1);
            drop(view);
        }
        sessions.close();
        other_account.close();
        assert!(sessions.context(Provider::Youtube).is_err());
        // An OS-owned destruction invalidates the Rust HWND guard as well.
        let destroyed = parent_window();
        unsafe { DestroyWindow(destroyed.handle()) };
        assert!(destroyed.handle().is_null());
        let unrelated = parent_window();
        drop(destroyed);
        assert_ne!(unsafe { IsWindow(unrelated.handle()) }, 0);
        assert_delayed_login_completion(&origin, &parent, &root);
        assert_cursor_restored_before_login_disappears(&origin, &parent);
        stopped.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        assert_cross_site_viewing(&root, &parent);
        eprintln!("Native Windows provider sessions: private profile identity; no login auth/IPC/scripts/opener; WM_CLOSE controller teardown; empty keeper across last POV close; provider/account isolation; disconnect/profile retirement; stale HWND ownership passed");
    }

    fn assert_cross_site_viewing(root: &std::path::Path, parent: &NativeWindow) {
        use super::super::session::{Cookie, Jar};
        use webview2_com::Microsoft::Web::WebView2::Win32::{
            ICoreWebView2_3, COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_DENY_CORS,
        };
        use windows::core::HSTRING;

        let origin = url::Url::parse("https://brick-fixture.test/").unwrap();
        for provider in [Provider::Youtube, Provider::Twitch] {
            let (host, domain, names) = match provider {
                Provider::Youtube => ("www.youtube.com", ".youtube.com", vec!["SID", "HSID"]),
                Provider::Twitch => ("player.twitch.tv", ".twitch.tv", vec!["auth-token"]),
            };
            let pages = root.join(format!("cross-site-{}", provider.label()));
            std::fs::create_dir_all(&pages).unwrap();
            // These mappings are confined to this offline fixture, which has
            // never visited a real provider and only uses synthetic cookies.
            std::fs::write(
                pages.join("frame.html"),
                r#"<!doctype html><script>
const result={cookie:document.cookie.includes('brick_fixture_viewing=fixture-only'),
 httpOnlyHidden:!document.cookie.includes('brick_fixture_http_only'),
 strict:document.cookie.includes('brick_fixture_strict=fixture-only')};
if(parent===window)document.title=JSON.stringify(result);
else parent.postMessage(result,new URL(document.referrer).origin);
</script>"#,
            )
            .unwrap();
            std::fs::write(pages.join("wrapper.html"), format!(r#"<!doctype html><script>
addEventListener('message',e=>{{if(e.origin==='https://{host}'&&e.source===document.querySelector('iframe').contentWindow)document.title=JSON.stringify(e.data);}});
</script><iframe src="https://{host}/frame.html"></iframe>"#)).unwrap();
            let cookie = |name: &str, http_only, same_site| Cookie {
                name: name.into(),
                value: "fixture-only".into(),
                domain: domain.into(),
                path: "/".into(),
                expires: Some(super::super::session::now() + 3600),
                secure: true,
                http_only,
                same_site,
            };
            let mut saved = vec![
                cookie("brick_fixture_viewing", false, 0),
                cookie("brick_fixture_http_only", true, 0),
                cookie("brick_fixture_strict", false, 2),
            ];
            saved.extend(names.into_iter().map(|name| cookie(name, true, 0)));
            let account = "987654321-cross-site-fixture";
            let jar = Jar::new(provider.clone(), account);
            wait_for("empty protected viewing jar", || jar.ready());
            assert!(jar.error().is_none());
            jar.observe(saved.clone());
            drop(jar); // Finish the DPAPI write before simulating restart.
            let jar = Jar::new(provider.clone(), account);
            wait_for("DPAPI viewing restore", || jar.ready());
            assert!(jar.error().is_none());
            assert!(jar.signed_in());
            assert_eq!(jar.cookies().len(), saved.len());

            let context = Context::for_origin(&provider, Some(&origin)).unwrap();
            context.restore(&jar.cookies()).unwrap();
            let map = |view: &WebView| {
                let core = view.webview().cast::<ICoreWebView2_3>().unwrap();
                for host in [host, "brick-fixture.test", "outside-fixture.test"] {
                    unsafe {
                        core.SetVirtualHostNameToFolderMapping(
                            &HSTRING::from(host),
                            &HSTRING::from(pages.to_string_lossy().as_ref()),
                            COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_DENY_CORS,
                        )
                    }
                    .unwrap();
                }
            };
            let login = media_view(&context, parent, root);
            map(&login);
            login
                .load_url(&format!("https://{host}/frame.html"))
                .unwrap();
            let first = state(&login, "first-party synthetic provider login");
            assert_eq!(first["cookie"], true);
            assert_eq!(first["strict"], true);
            assert_eq!(first["httpOnlyHidden"], true);
            context.keep_alive(parent.handle()).unwrap();
            drop(login);
            let snapshot = |context: &Context, origin: &str| {
                let view = media_view(context, parent, root);
                map(&view);
                view.load_url(&format!("{origin}/wrapper.html")).unwrap();
                state(&view, "cross-site synthetic player")
            };
            let embedded = snapshot(&context, "https://brick-fixture.test");
            assert_eq!(
                embedded["cookie"],
                true,
                "{} viewing login must reach its iframe",
                provider.label()
            );
            assert_eq!(embedded["strict"], false, "SameSite remains enforced");
            assert_eq!(
                embedded["httpOnlyHidden"], true,
                "HttpOnly remains enforced"
            );
            assert_eq!(
                snapshot(&context, "https://outside-fixture.test")["cookie"],
                false,
                "unrelated embedding sites stay blocked"
            );
            let other_provider = match provider {
                Provider::Youtube => Provider::Twitch,
                Provider::Twitch => Provider::Youtube,
            };
            let other = Context::for_origin(&other_provider, Some(&origin)).unwrap();
            // Even cookies placed in the wrong synthetic profile must not get
            // an exception for the other provider's domain.
            other.restore(&saved).unwrap();
            assert_eq!(
                snapshot(&other, "https://brick-fixture.test")["cookie"],
                false
            );
            other.retire();
            context.retire();
            jar.clear();
            drop(jar);
            let cleared = Jar::new(provider.clone(), account);
            wait_for("protected sign-out removal", || cleared.ready());
            assert!(cleared.error().is_none());
            assert!(!cleared.signed_in());
            assert!(cleared.cookies().is_empty());
            let signed_out = Context::for_origin(&provider, Some(&origin)).unwrap();
            signed_out.restore(&cleared.cookies()).unwrap();
            assert_eq!(
                snapshot(&signed_out, "https://brick-fixture.test")["cookie"],
                false
            );
            signed_out.retire();
            eprintln!("{}: private cross-site login, DPAPI restore, provider/site isolation, SameSite/HttpOnly and sign-out passed", provider.label());
        }
    }

    // No credentials or provider traffic: cookie creation is deliberately delayed
    // until after the return page finishes, reproducing post-verification setup.
    fn assert_delayed_login_completion(
        origin: &str,
        parent: &NativeWindow,
        root: &std::path::Path,
    ) {
        let ctx = egui::Context::default();
        for provider in [Provider::Youtube, Provider::Twitch] {
            let sessions = ProviderSessions::default();
            let context = sessions.context(provider.clone()).unwrap();
            context
                .open_with(|| {
                    Window::new_at_owner(
                        &context.platform,
                        &provider,
                        parent.handle(),
                        &ctx,
                        &format!("{origin}/login"),
                        fixture_origin,
                        |_, value| value.ends_with("/returned"),
                    )
                })
                .unwrap();
            {
                let window = context.window.borrow();
                let window = window.as_ref().unwrap();
                wait_for("login loaded before delayed return", || {
                    page_title(&window.view).is_some_and(|title| title.starts_with('{'))
                });
                window.view.load_url(&format!("{origin}/returned")).unwrap();
                wait_for("delayed return loaded", || window.completion.returned.get());
                assert!(!(unsafe { IsWindowVisible(window.native.handle()) != 0 }));
                assert!(window.open(), "hiding must keep the browser alive");
            }
            context.poll_session(&ctx);
            wait_for("unauthenticated return observation", || {
                !context.polling.get()
            });
            context.poll_session(&ctx);
            assert!(context.window_open());
            assert!(!sessions.signed_in(&provider));
            {
                let window = context.window.borrow();
                let window = window.as_ref().unwrap();
                window
                    .completion
                    .hidden_since
                    .set(Some(Instant::now() - Duration::from_secs(31)));
            }
            context.poll_session(&ctx);
            {
                let window = context.window.borrow();
                let window = window.as_ref().unwrap();
                assert!(
                    unsafe { IsWindowVisible(window.native.handle()) != 0 },
                    "an unsuccessful return remains accessible"
                );
                window.view.load_url(&format!("{origin}/returned")).unwrap();
                wait_for("return hidden again", || {
                    window.completion.returned.get()
                        && window.completion.hidden_since.get().is_some()
                });
            }
            let (domain, names) = match provider {
                Provider::Youtube => (".youtube.com", vec!["SID", "HSID"]),
                Provider::Twitch => (".twitch.tv", vec!["auth-token"]),
            };
            let cookies: Vec<_> = names
                .into_iter()
                .map(|name| super::super::session::Cookie {
                    name: name.into(),
                    value: "synthetic-delayed-login".into(),
                    domain: domain.into(),
                    path: "/".into(),
                    expires: Some(super::super::session::now() + 3600),
                    secure: true,
                    http_only: true,
                    same_site: 1,
                })
                .collect();
            context.platform.restore(&cookies).unwrap();
            let _media = media_view(&context.platform, parent, root);
            context.next_poll.set(None);
            wait_for("fresh authentication closes return", || {
                context.poll_session(&ctx);
                !context.window_open()
            });
            assert!(sessions.signed_in(&provider));
            sessions.close();
            eprintln!("{}: hidden return stayed alive until fresh auth cookies; retry timeout and delayed sign-in passed", provider.label());
        }
    }

    fn assert_cursor_restored_before_login_disappears(origin: &str, parent: &NativeWindow) {
        use windows_sys::Win32::{
            Foundation::POINT,
            Graphics::Gdi::ClientToScreen,
            UI::{
                Input::KeyboardAndMouse::{
                    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_A,
                },
                WindowsAndMessaging::{
                    SetCursorPos, ShowCursor, SystemParametersInfoW, SPI_GETMOUSEVANISH,
                },
            },
        };
        // Only the explicitly isolated native fixture runs this. We read the
        // desktop setting; neither the app nor this fixture changes it.
        let mut vanish: i32 = 0;
        assert_ne!(
            unsafe {
                SystemParametersInfoW(SPI_GETMOUSEVANISH, 0, (&mut vanish as *mut i32).cast(), 0)
            },
            0
        );
        assert_ne!(
            vanish, 0,
            "Cursor fixture needs hide-pointer-while-typing enabled"
        );
        let count = || unsafe {
            // Read the UI thread's cursor count with a balanced round trip.
            // Production never changes ShowCursor's count itself.
            let prior = ShowCursor(1) - 1;
            ShowCursor(0);
            prior
        };
        unsafe {
            ShowWindow(parent.handle(), SW_SHOW);
        }
        for provider in [Provider::Youtube, Provider::Twitch] {
            for mode in ["return", "close", "drop"] {
                let sessions = ProviderSessions::default();
                let context = sessions.context(provider.clone()).unwrap();
                let login = Window::new_at_owner(
                    &context.platform,
                    &provider,
                    parent.handle(),
                    &egui::Context::default(),
                    &format!("{origin}/cursor"),
                    fixture_origin,
                    |_, value| value.ends_with("/returned"),
                )
                .unwrap();
                wait_for("synthetic typing page ready", || {
                    page_title(&login.view).as_deref() == Some("cursor-ready")
                });
                login.present();
                login.view.focus().unwrap();
                let mut point = POINT { x: 40, y: 36 };
                assert_ne!(
                    unsafe { ClientToScreen(login.native.handle(), &mut point) },
                    0
                );
                assert_ne!(unsafe { SetCursorPos(point.x, point.y) }, 0);
                wait_for("pointer restored on entering sign-in", || count() >= 0);
                // Drain the native move before typing; a move delivered after
                // the key would correctly cancel hide-while-typing again.
                let settled = Instant::now() + Duration::from_millis(250);
                wait_for("synthetic pointer movement settled", || {
                    Instant::now() >= settled
                });
                let baseline = count();
                let keys = [0, KEYEVENTF_KEYUP].map(|flags| INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VK_A,
                            dwFlags: flags,
                            ..Default::default()
                        },
                    },
                });
                assert_eq!(
                    unsafe {
                        SendInput(
                            keys.len() as u32,
                            keys.as_ptr(),
                            std::mem::size_of::<INPUT>() as i32,
                        )
                    },
                    2
                );
                wait_for("synthetic typing hides pointer", || {
                    page_title(&login.view).as_deref() == Some("cursor-typed") && count() < baseline
                });
                assert_eq!(count(), baseline - 1);
                let handle = login.native.handle();
                *context.window.borrow_mut() = Some(login);
                match mode {
                    "return" => {
                        {
                            let window = context.window.borrow();
                            window
                                .as_ref()
                                .unwrap()
                                .view
                                .load_url(&format!("{origin}/returned"))
                                .unwrap();
                        }
                        wait_for("automatic return hides without cursor leak", || {
                            context
                                .window
                                .borrow()
                                .as_ref()
                                .unwrap()
                                .completion
                                .returned
                                .get()
                        });
                        assert_eq!(unsafe { IsWindowVisible(handle) }, 0);
                        assert!(
                            context.window_open(),
                            "auth completion still retains hidden browser"
                        );
                        assert_eq!(
                            count(),
                            baseline,
                            "automatic return must release typing suppression before hiding"
                        );
                        context.close_window();
                    }
                    "close" => {
                        unsafe {
                            SendMessageW(handle, WM_CLOSE, 0, 0);
                        }
                        assert!(!context.window_open());
                    }
                    "drop" => context.close_window(),
                    _ => unreachable!(),
                }
                wait_for("native app cursor count restored after login", || {
                    count() == baseline
                });
                // Entering another browser must not increment a stale count.
                // The next iteration checks the same visible baseline again.
                assert_eq!(
                    baseline, 0,
                    "repeated sign-ins must not accumulate cursor adjustments"
                );
                sessions.close();
                eprintln!(
                    "{}: {mode} restores cursor after actual synthetic typing",
                    provider.label()
                );
            }
        }
        // Hiding an inactive login must not bring Brick in front of another
        // native window. No synthetic credentials or typing in this case.
        let context = Context::new(&Provider::Youtube).unwrap();
        let login = Window::new_at_owner(
            &context,
            &Provider::Youtube,
            parent.handle(),
            &egui::Context::default(),
            &format!("{origin}/cursor"),
            fixture_origin,
            |_, _| false,
        )
        .unwrap();
        wait_for("inactive cursor fixture page", || {
            page_title(&login.view).as_deref() == Some("cursor-ready")
        });
        let other = parent_window();
        unsafe {
            ShowWindow(other.handle(), SW_SHOW);
            SetForegroundWindow(other.handle());
            SetFocus(other.handle());
        }
        wait_for(
            "another native window has focus",
            || unsafe { GetForegroundWindow() } == other.handle(),
        );
        login.native.0.hide();
        drop(login);
        assert_eq!(unsafe { GetForegroundWindow() }, other.handle());
        context.retire();
    }
}
