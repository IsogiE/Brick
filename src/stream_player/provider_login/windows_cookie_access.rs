//! Configure only a fresh, app-owned profile, before WebView2 opens it.
//!
//! WebView2 InPrivate blocks third-party cookies independently of tracking
//! prevention. It currently has no public API for cookie exceptions. Seed the
//! Chromium preference for this provider embedded by Brick; never edit an
//! existing profile, a machine policy, or any cookie attribute. The native
//! cross-site fixture must cover this runtime-dependent preference contract.
use crate::streams::Provider;
use std::{fs, io::Write, path::PathBuf};
use url::Url;

pub(super) fn prepare(
    name: &str,
    provider: &Provider,
    origin: Option<&Url>,
) -> Result<PathBuf, String> {
    let suffix = name.strip_prefix("BrickViewer").unwrap_or("");
    if suffix.len() != 32 || !suffix.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("Invalid private viewing profile.".into());
    }
    let mut exceptions = serde_json::Map::new();
    if let Some(origin) = origin {
        let host = origin.host_str().unwrap_or("");
        if !(origin.scheme() == "https" || (origin.scheme() == "http" && host == "127.0.0.1"))
            || !origin.username().is_empty()
            || origin.password().is_some()
            || host.is_empty()
            || host.contains(['*', ','])
        {
            return Err("Invalid viewing player origin.".into());
        }
        let provider = match provider {
            Provider::Youtube => "https://[*.]youtube.com",
            Provider::Twitch => "https://[*.]twitch.tv",
        };
        exceptions.insert(
            format!("{provider},{}", origin.origin().ascii_serialization()),
            serde_json::json!({"setting": 1}),
        );
    }
    let payload = serde_json::to_vec(&serde_json::json!({
        "profile": {"content_settings": {"exceptions": {"cookies": exceptions}}}
    }))
    .map_err(|_| "The viewing player cookie settings could not be prepared.")?;
    let parent = super::super::super::windows_profile::data_directory()?.join("EBWebView");
    fs::create_dir_all(&parent)
        .map_err(|_| "The private viewing profile could not be prepared.")?;
    // WebView2 canonicalizes non-default profile names this way. register()
    // verifies the runtime-reported path before restoring any saved cookies.
    let path = parent.join(format!("WV2Profile_{}", name.to_ascii_lowercase()));
    fs::create_dir(&path).map_err(|_| "The private viewing profile must be newly created.")?;
    let result = (|| -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path.join("Preferences"))?;
        file.write_all(&payload)?;
        file.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(path.join("Preferences"));
        let _ = fs::remove_dir(&path);
        return Err("The private viewing cookie settings could not be saved.".into());
    }
    Ok(path)
}
