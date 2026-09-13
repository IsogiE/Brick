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
    NavigationStartingEventHandler,
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

    fn keep_alive(&self, player: &WebView, owner: HWND) -> Result<(), String> {
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
        let view = WebViewBuilder::new()
            .with_environment(player.environment())
            .with_profile_name(&self.profile_name)
            .with_incognito(true)
            .with_visible(false)
            .with_devtools(false)
            .with_clipboard(false)
            .with_navigation_handler(|_| false)
            .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
            .with_download_started_handler(|_, _| false)
            .build(&native)
            .map_err(|_| "The private provider session could not start.")?;
        protect_settings(&view)?;
        super::super::protect_windows_permissions(&view)?;
        self.register(&view)?;
        *self.keeper.borrow_mut() = Some(Keeper {
            _view: view,
            _native: native,
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
            RemoveWindowSubclass(hwnd, Some(window_lifetime), id);
            drop(Rc::from_raw(data as *const WindowState));
        }
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

struct Keeper {
    _view: WebView,
    _native: NativeWindow,
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
    pub fn new(
        context: &Context,
        provider: &Provider,
        player: &WebView,
        ctx: &egui::Context,
    ) -> Result<Self, String> {
        let mut owner = windows::Win32::Foundation::HWND::default();
        unsafe { player.controller().ParentWindow(&mut owner) }
            .map_err(|_| "The provider sign-in window could not find Brick.")?;
        let owner = unsafe { GetAncestor(owner.0, GA_ROOT) };
        if owner.is_null() {
            return Err("The provider sign-in window could not find Brick.".into());
        }
        context.keep_alive(player, owner)?;
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
            .with_environment(player.environment())
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
            .with_navigation_handler(move |url| allowed_document(&nav_provider, &url))
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
                    if allowed_document(&frame_provider, &uri) {
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
        view.load_url(start_url(provider))
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
