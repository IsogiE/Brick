use super::{allowed_document, start_url, title};
use crate::streams::Provider;
use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle};
use std::{
    cell::{Cell, RefCell},
    num::NonZeroIsize,
    rc::Rc,
};
use webview2_com::{
    Microsoft::Web::WebView2::Win32::{
        ICoreWebView2Controller, ICoreWebView2Environment, ICoreWebView2Profile6,
        ICoreWebView2Profile8, ICoreWebView2_13,
    },
    NavigationStartingEventHandler, NewWindowRequestedEventHandler,
};
use windows::core::{Interface, PWSTR};
use windows::Win32::{
    Foundation::{HWND as WinHwnd, LPARAM, LRESULT, WPARAM},
    UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
};
use windows_sys::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetAncestor, GetClientRect, GetSystemMetrics,
        IsWindowVisible, SetForegroundWindow, SetWindowTextW, ShowWindow, GA_ROOT, SM_CXSCREEN,
        SM_CYSCREEN, SW_HIDE, SW_SHOW, WM_CLOSE, WM_NCDESTROY, WS_CAPTION, WS_CLIPCHILDREN,
        WS_POPUP, WS_SYSMENU,
    },
};
use wry::{WebView, WebViewBuilder, WebViewBuilderExtWindows, WebViewExtWindows};

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
    pub environment: RefCell<Option<ICoreWebView2Environment>>,
    profile: RefCell<Option<ICoreWebView2Profile8>>,
    keeper: RefCell<Option<Keeper>>,
}

impl Context {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            profile_name: format!("BrickViewer{}", uuid::Uuid::new_v4().simple()),
            environment: RefCell::new(None),
            profile: RefCell::new(None),
            keeper: RefCell::new(None),
        })
    }

    pub fn register(&self, view: &WebView) -> Result<(), String> {
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
        *self.environment.borrow_mut() = Some(view.environment());
        *self.profile.borrow_mut() = Some(managed);
        Ok(())
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
        self.register(&view)?;
        *self.keeper.borrow_mut() = Some(Keeper {
            _view: view,
            _native: native,
            _storage: storage,
        });
        Ok(())
    }

    pub fn retire(&self) {
        self.keeper.borrow_mut().take();
        if let Some(profile) = self.profile.borrow_mut().take() {
            // Closes every related WebView. Runtime removes the owned profile
            // at browser exit, retrying deletion on later starts if necessary.
            let _ = unsafe { profile.Delete() };
        }
        self.environment.borrow_mut().take();
    }
}

struct WindowState {
    handle: Cell<HWND>,
    closed: Cell<bool>,
    controller: RefCell<Option<ICoreWebView2Controller>>,
    repaint: Option<egui::Context>,
}

struct NativeWindow(Rc<WindowState>);

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
        unsafe { ShowWindow(hwnd.0, SW_HIDE) };
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
    // Drop the WebView before its native parent.
    view: WebView,
    native: NativeWindow,
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
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
        )
    }

    fn new_at(
        context: &Context,
        provider: &Provider,
        player: &WebView,
        ctx: &egui::Context,
        start: &str,
        permits: fn(&Provider, &str) -> bool,
    ) -> Result<Self, String> {
        let mut owner = windows::Win32::Foundation::HWND::default();
        unsafe { player.controller().ParentWindow(&mut owner) }
            .map_err(|_| "The provider sign-in window could not find Brick.")?;
        Self::new_at_owner(context, provider, owner.0, ctx, start, permits)
    }

    fn new_at_owner(
        context: &Context,
        provider: &Provider,
        owner: HWND,
        ctx: &egui::Context,
        start: &str,
        permits: fn(&Provider, &str) -> bool,
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
        let width = unsafe { GetSystemMetrics(SM_CXSCREEN) }.clamp(320, 580);
        let height = unsafe { GetSystemMetrics(SM_CYSCREEN) }
            .saturating_sub(80)
            .clamp(300, 740);
        let caption = wide(&title(provider, start_url(provider)));
        let class = wide("STATIC");
        // Fixed-size native window. The lifetime subclass keeps HWND ownership
        // valid when the close button ends only this login controller.
        let handle = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                caption.as_ptr(),
                WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN,
                80,
                50,
                width,
                height,
                owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if handle.is_null() {
            return Err("The provider sign-in window could not open.".into());
        }
        let native = NativeWindow::new(handle, Some(ctx))?;
        let mut rect = windows_sys::Win32::Foundation::RECT::default();
        if unsafe { GetClientRect(handle, &mut rect) } == 0 {
            return Err("The provider sign-in window could not size its browser.".into());
        }
        let nav_provider = provider.clone();
        let title_provider = provider.clone();
        let title_window = Rc::downgrade(&native.0);
        let view = WebViewBuilder::new()
            .with_environment(environment)
            .with_profile_name(&context.profile_name)
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
            .with_navigation_handler(move |url| permits(&nav_provider, &url))
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
        context.register(&view)?;
        *native.0.controller.borrow_mut() = Some(view.controller());
        view.load_url(start)
            .map_err(|_| "The provider sign-in page could not open.")?;
        unsafe {
            ShowWindow(handle, SW_SHOW);
            SetForegroundWindow(handle);
        }
        Ok(Self { view, native })
    }

    pub fn open(&self) -> bool {
        !self.native.0.closed.get()
            && !self.native.handle().is_null()
            && unsafe { IsWindowVisible(self.native.handle()) != 0 }
    }
    pub fn present(&self) {
        unsafe {
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
        DispatchMessageW, GetForegroundWindow, IsWindow, PeekMessageW, SendMessageW,
        TranslateMessage, MSG, PM_REMOVE,
    };

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
        NativeWindow::new(handle, None).unwrap()
    }

    fn media_view(context: &Context, parent: &NativeWindow, root: &std::path::Path) -> WebView {
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
        context.register(&view).unwrap();
        view
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
        )
        .unwrap();
        let initial = state(&login.view, "initial sign-in page");
        login.present();
        wait_for(
            "initial sign-in foreground",
            || unsafe { GetForegroundWindow() } == login.native.handle(),
        );
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
        let twitch = sessions.context(Provider::Twitch).unwrap();
        assert_ne!(youtube.platform.profile_name, twitch.platform.profile_name);
        assert_eq!(snapshot(&twitch)["cookie"], false);
        let other_account = ProviderSessions::default();
        let other_youtube = other_account.context(Provider::Youtube).unwrap();
        assert_eq!(snapshot(&other_youtube)["cookie"], false);
        let active_pov = media_view(&youtube.platform, &parent, &root);
        let reopened = Window::new_at(
            &youtube.platform,
            &Provider::Youtube,
            &active_pov,
            &egui::Context::default(),
            &format!("{origin}/state"),
            fixture_origin,
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
        // Profile deletion closes its views through WebView2's Deleted event.
        // Pump that bounded asynchronous teardown before checking retirement.
        wait_for("disconnected profile view closure", || {
            let mut source = PWSTR::null();
            if unsafe { active_pov.webview().Source(&mut source) }.is_err() {
                true
            } else {
                let _ = webview2_com::take_pwstr(source);
                false
            }
        });
        assert!(twitch.active());
        assert_eq!(snapshot(&twitch)["cookie"], false);
        let replacement = sessions.context(Provider::Youtube).unwrap();
        assert_ne!(
            replacement.platform.profile_name,
            youtube.platform.profile_name
        );
        assert_eq!(snapshot(&replacement)["cookie"], false);
        assert_eq!(snapshot(&replacement)["storage"], false);
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
        stopped.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        eprintln!("Native Windows provider sessions: private profile identity; no login auth/IPC/scripts/opener; WM_CLOSE controller teardown; empty keeper across last POV close; provider/account isolation; disconnect/profile retirement; stale HWND ownership passed");
    }
}
