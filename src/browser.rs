use std::process::Command;

pub fn open(url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn()
            .map_err(|error| format!("Failed to open your browser: {error}"))?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|error| format!("Failed to open your browser: {error}"))?;
        return Ok(());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for command in ["xdg-open", "gio", "kde-open", "gnome-open"] {
            let result = if command == "gio" {
                Command::new(command).args(["open", url]).spawn()
            } else {
                Command::new(command).arg(url).spawn()
            };

            if result.is_ok() {
                return Ok(());
            }
        }

        Err("Failed to open your browser.".to_string())
    }
}
