use eframe::egui;

#[derive(Debug, Clone, Copy)]
pub enum TrayCommand {
    Show,
    #[cfg(not(target_os = "windows"))]
    Quit,
}

#[cfg(target_os = "windows")]
mod native_window {
    use std::sync::atomic::{AtomicIsize, Ordering};

    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    use windows_sys::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{IsWindow, SetForegroundWindow, ShowWindowAsync, SW_RESTORE},
    };

    static MAIN_WINDOW: AtomicIsize = AtomicIsize::new(0);

    pub fn remember(frame: &eframe::Frame) {
        let Ok(handle) = frame.window_handle() else {
            return;
        };

        if let RawWindowHandle::Win32(handle) = handle.as_raw() {
            let hwnd = handle.hwnd.get();
            if MAIN_WINDOW.swap(hwnd, Ordering::SeqCst) != hwnd {
                let _ = crate::single_instance::remember_main_window_handle(hwnd);
            }
        }
    }

    pub fn show() {
        let hwnd = MAIN_WINDOW.load(Ordering::SeqCst);
        if hwnd == 0 {
            return;
        }

        let hwnd = hwnd as HWND;
        unsafe {
            if IsWindow(hwnd) == 0 {
                return;
            }

            ShowWindowAsync(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        }
    }
}

#[cfg(target_os = "windows")]
pub fn remember_main_window(frame: &eframe::Frame) {
    native_window::remember(frame);
}

#[cfg(not(target_os = "windows"))]
pub fn remember_main_window(_frame: &eframe::Frame) {}

#[cfg(target_os = "linux")]
mod platform {
    use std::{
        sync::{mpsc, LazyLock},
        thread,
    };

    use ksni::{blocking::TrayMethods, menu::StandardItem, Category, Icon, MenuItem, Status, Tray};

    use super::TrayCommand;
    use eframe::egui;

    const ICON_BYTES: &[u8] = include_bytes!("assets/brick.png");

    static ICON_PIXMAP: LazyLock<Vec<Icon>> = LazyLock::new(|| {
        let Ok(image) = image::load_from_memory(ICON_BYTES) else {
            return Vec::new();
        };
        let image = image
            .resize_exact(32, 32, image::imageops::FilterType::Lanczos3)
            .to_rgba8();
        let (width, height) = image.dimensions();
        let mut data = image.into_raw();
        for pixel in data.chunks_exact_mut(4) {
            pixel.rotate_right(1);
        }
        vec![Icon {
            width: width as i32,
            height: height as i32,
            data,
        }]
    });

    pub struct TrayState {
        _handle: ksni::blocking::Handle<BrickTray>,
        rx: mpsc::Receiver<TrayCommand>,
    }

    impl TrayState {
        pub fn drain_commands(&self) -> Vec<TrayCommand> {
            let mut commands = Vec::new();
            while let Ok(command) = self.rx.try_recv() {
                commands.push(command);
            }
            commands
        }
    }

    struct BrickTray {
        tx: mpsc::Sender<TrayCommand>,
        ctx: egui::Context,
    }

    impl BrickTray {
        fn send(&self, command: TrayCommand) {
            let _ = self.tx.send(command);
            self.ctx.request_repaint();
        }
    }

    impl Tray for BrickTray {
        fn id(&self) -> String {
            "brick".to_string()
        }

        fn title(&self) -> String {
            "Brick".to_string()
        }

        fn category(&self) -> Category {
            Category::ApplicationStatus
        }

        fn status(&self) -> Status {
            Status::Active
        }

        fn icon_name(&self) -> String {
            "brick".to_string()
        }

        fn icon_pixmap(&self) -> Vec<Icon> {
            ICON_PIXMAP.clone()
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            self.send(TrayCommand::Show);
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            vec![
                StandardItem {
                    label: "Open Brick".to_string(),
                    activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Show)),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: "Quit Brick".to_string(),
                    activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Quit)),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    pub fn create(ctx: egui::Context) -> Result<TrayState, String> {
        let (tx, rx) = mpsc::channel();
        let tray = BrickTray { tx, ctx };
        let handle = tray
            .assume_sni_available(true)
            .spawn()
            .map_err(|error| format!("Failed to create SNI tray icon: {error}"))?;

        thread::sleep(std::time::Duration::from_millis(25));
        Ok(TrayState {
            _handle: handle,
            rx,
        })
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use std::sync::mpsc;

    use tray_icon::{
        menu::{Menu, MenuEvent, MenuItem},
        Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    };

    use super::TrayCommand;
    use eframe::egui;

    const ICON_BYTES: &[u8] = include_bytes!("assets/brick.png");

    pub struct TrayState {
        _tray: TrayIcon,
        rx: mpsc::Receiver<TrayCommand>,
    }

    impl TrayState {
        pub fn drain_commands(&self) -> Vec<TrayCommand> {
            let mut commands = Vec::new();
            while let Ok(command) = self.rx.try_recv() {
                commands.push(command);
            }
            commands
        }
    }

    pub fn create(ctx: egui::Context) -> Result<TrayState, String> {
        let (tx, rx) = mpsc::channel();
        let menu = Menu::new();
        let show = MenuItem::with_id("show", "Open Brick", true, None);
        let quit = MenuItem::with_id("quit", "Quit Brick", true, None);
        menu.append_items(&[&show, &quit])
            .map_err(|error| format!("Failed to build tray menu: {error}"))?;

        let menu_tx = tx.clone();
        let menu_ctx = ctx.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let command = match event.id.as_ref() {
                "show" => Some(TrayCommand::Show),
                "quit" => {
                    #[cfg(target_os = "windows")]
                    {
                        std::process::exit(0);
                    }
                    #[cfg(not(target_os = "windows"))]
                    {
                        Some(TrayCommand::Quit)
                    }
                }
                _ => None,
            };
            if let Some(command) = command {
                if matches!(command, TrayCommand::Show) {
                    #[cfg(target_os = "windows")]
                    super::native_window::show();
                }
                let _ = menu_tx.send(command);
                menu_ctx.request_repaint();
            }
        }));

        let tray_tx = tx.clone();
        let tray_ctx = ctx.clone();
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            let show = matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            );
            if show {
                #[cfg(target_os = "windows")]
                super::native_window::show();
                let _ = tray_tx.send(TrayCommand::Show);
                tray_ctx.request_repaint();
            }
        }));

        let tray = TrayIconBuilder::new()
            .with_tooltip("Brick")
            .with_icon(tray_icon()?)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .map_err(|error| format!("Failed to create tray icon: {error}"))?;

        Ok(TrayState { _tray: tray, rx })
    }

    fn tray_icon() -> Result<Icon, String> {
        let image = image::load_from_memory(ICON_BYTES)
            .map_err(|error| format!("Failed to load tray icon: {error}"))?
            .resize_exact(32, 32, image::imageops::FilterType::Lanczos3)
            .to_rgba8();
        let (width, height) = image.dimensions();
        Icon::from_rgba(image.into_raw(), width, height)
            .map_err(|error| format!("Failed to prepare tray icon: {error}"))
    }
}

pub fn create(ctx: egui::Context) -> Result<TrayState, String> {
    platform::create(ctx)
}

pub use platform::TrayState;
