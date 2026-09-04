use eframe::egui;

#[derive(Debug, Clone, Copy)]
pub enum TrayCommand {
    Show,
    Quit,
}

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
        MenuEvent::set_event_handler(Some(move |event| {
            let command = match event.id.as_ref() {
                "show" => Some(TrayCommand::Show),
                "quit" => Some(TrayCommand::Quit),
                _ => None,
            };
            if let Some(command) = command {
                let _ = menu_tx.send(command);
                menu_ctx.request_repaint();
            }
        }));

        let tray_tx = tx.clone();
        let tray_ctx = ctx.clone();
        TrayIconEvent::set_event_handler(Some(move |event| {
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
