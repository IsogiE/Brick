#[derive(Debug, Clone, Copy)]
pub enum TrayCommand {
    Show,
    Hide,
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
    }

    impl BrickTray {
        fn send(&self, command: TrayCommand) {
            let _ = self.tx.send(command);
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
                    label: "Show Brick".to_string(),
                    activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Show)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: "Hide".to_string(),
                    activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Hide)),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: "Quit".to_string(),
                    activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Quit)),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    pub fn create() -> Result<TrayState, String> {
        let (tx, rx) = mpsc::channel();
        let tray = BrickTray { tx };
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
    use tray_icon::{
        menu::{Menu, MenuEvent, MenuItem},
        Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
    };

    use super::TrayCommand;

    const ICON_BYTES: &[u8] = include_bytes!("assets/brick.png");

    pub struct TrayState {
        _tray: TrayIcon,
    }

    impl TrayState {
        pub fn drain_commands(&self) -> Vec<TrayCommand> {
            let mut commands = Vec::new();

            while let Ok(event) = MenuEvent::receiver().try_recv() {
                match event.id.as_ref() {
                    "show" => commands.push(TrayCommand::Show),
                    "hide" => commands.push(TrayCommand::Hide),
                    "quit" => commands.push(TrayCommand::Quit),
                    _ => {}
                }
            }

            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if matches!(event, TrayIconEvent::Click { .. }) {
                    commands.push(TrayCommand::Show);
                }
            }

            commands
        }
    }

    pub fn create() -> Result<TrayState, String> {
        let menu = Menu::new();
        let show = MenuItem::with_id("show", "Show Brick", true, None);
        let hide = MenuItem::with_id("hide", "Hide", true, None);
        let quit = MenuItem::with_id("quit", "Quit", true, None);
        menu.append_items(&[&show, &hide, &quit])
            .map_err(|error| format!("Failed to build tray menu: {error}"))?;

        let tray = TrayIconBuilder::new()
            .with_tooltip("Brick")
            .with_icon(tray_icon()?)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(true)
            .build()
            .map_err(|error| format!("Failed to create tray icon: {error}"))?;

        Ok(TrayState { _tray: tray })
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

pub fn create() -> Result<TrayState, String> {
    platform::create()
}

pub use platform::TrayState;
