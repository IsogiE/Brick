use std::{
    collections::HashSet,
    convert::TryInto,
    env,
    fs::{self, OpenOptions},
    io::{self, Cursor, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;
use zip::ZipArchive;

const APP_ID: &str = "dev.isogi.brick";
const FEED_OWNER: &str = "IsogiE";
const FEED_REPO: &str = "Brick-Releases";
const FEED_TAG: &str = "addon-feed";
const PACKAGE_ID: &str = "AdvanceRaidTools";
const FEED_URL: &str =
    "https://github.com/IsogiE/Brick-Releases/releases/download/addon-feed/addon-manifest.json";
const FEED_SIG_URL: &str =
    "https://github.com/IsogiE/Brick-Releases/releases/download/addon-feed/addon-manifest.json.sig";
const APP_USER_AGENT: &str = "Brick/0.1 (+https://github.com/IsogiE/Brick-Releases)";
const FEED_UNAVAILABLE_MESSAGE: &str =
    "No signed addon feed is available yet. Brick will check again automatically.";
const SETTINGS_FILE: &str = "settings.json";
const LOG_FILE: &str = "logs.jsonl";
const SYNC_INTERVAL_SECS: u64 = 5 * 60;
const ALLOWED_FOLDERS: &[&str] = &[
    "AdvanceRaidTools",
    "AdvanceRaidTools_Libraries",
    "AdvanceRaidTools_Options",
];

const ADDON_PUBLIC_KEY_B64: &str = match option_env!("BRICK_ADDON_PUBLIC_KEY_B64") {
    Some(value) => value,
    None => "",
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppView {
    pub settings: Settings,
    pub setup_required: bool,
    pub logs: Vec<LogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub schema: u32,
    pub watcher_enabled: bool,
    pub startup_enabled: bool,
    pub clients: Vec<WowClient>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema: 1,
            watcher_enabled: true,
            startup_enabled: true,
            clients: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WowClient {
    pub id: String,
    pub flavor: Flavor,
    pub path: String,
    #[serde(default)]
    pub game_version: Option<String>,
    pub last_installed_version: Option<String>,
    pub last_installed_sha256: Option<String>,
    pub last_sync_at: Option<String>,
}

impl WowClient {
    pub fn display_label(&self) -> String {
        let Some(version) = self.game_version.as_deref().and_then(short_game_version) else {
            return self.flavor.label().to_string();
        };

        format!("{} {version}", self.flavor.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Flavor {
    Retail,
    Ptr,
    Xptr,
    Beta,
    Classic,
    ClassicEra,
    ClassicPtr,
    ClassicBeta,
}

impl Flavor {
    pub fn label(self) -> &'static str {
        match self {
            Self::Retail => "Retail",
            Self::Ptr => "PTR",
            Self::Xptr => "PTR",
            Self::Beta => "Beta",
            Self::Classic => "Classic",
            Self::ClassicEra => "Classic Era",
            Self::ClassicPtr => "Classic PTR",
            Self::ClassicBeta => "Classic Beta",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Retail => "retail",
            Self::Ptr => "ptr",
            Self::Xptr => "xptr",
            Self::Beta => "beta",
            Self::Classic => "classic",
            Self::ClassicEra => "classic-era",
            Self::ClassicPtr => "classic-ptr",
            Self::ClassicBeta => "classic-beta",
        }
    }

    fn rank(self) -> usize {
        FLAVOR_INFOS
            .iter()
            .position(|info| info.flavor == self)
            .unwrap_or(usize::MAX)
    }
}

struct FlavorInfo {
    flavor: Flavor,
    directory: &'static str,
}

const FLAVOR_INFOS: &[FlavorInfo] = &[
    FlavorInfo {
        flavor: Flavor::Retail,
        directory: "_retail_",
    },
    FlavorInfo {
        flavor: Flavor::Ptr,
        directory: "_ptr_",
    },
    FlavorInfo {
        flavor: Flavor::Xptr,
        directory: "_xptr_",
    },
    FlavorInfo {
        flavor: Flavor::Beta,
        directory: "_beta_",
    },
    FlavorInfo {
        flavor: Flavor::Classic,
        directory: "_classic_",
    },
    FlavorInfo {
        flavor: Flavor::ClassicEra,
        directory: "_classic_era_",
    },
    FlavorInfo {
        flavor: Flavor::ClassicPtr,
        directory: "_classic_ptr_",
    },
    FlavorInfo {
        flavor: Flavor::ClassicBeta,
        directory: "_classic_beta_",
    },
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub at: String,
    pub level: LogLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSummary {
    pub version: Option<String>,
    pub checked_at: String,
    pub installed: usize,
    pub skipped: usize,
    pub message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddonManifest {
    schema: u32,
    package_id: String,
    version: String,
    commit: String,
    built_at: String,
    artifact: ManifestArtifact,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestArtifact {
    url: String,
    sha256: String,
    size: u64,
    folders: Vec<String>,
    #[serde(default)]
    flavors: Vec<Flavor>,
}

pub fn load_view() -> Result<AppView, String> {
    let mut settings = load_settings()?;
    maybe_auto_detect_clients(&mut settings)?;
    if refresh_client_metadata(&mut settings.clients) {
        save_settings(&settings)?;
    }
    view_from_settings(settings)
}

pub fn add_wow_paths(paths: &[PathBuf]) -> Result<AppView, String> {
    let mut new_clients = Vec::new();
    let mut seen = HashSet::new();
    let mut errors = Vec::new();

    for path in paths {
        match resolve_wow_clients(path) {
            Ok(clients) => {
                for client in clients {
                    if seen.insert(client.path.clone()) {
                        new_clients.push(client);
                    }
                }
            }
            Err(error) => errors.push(format!("{}: {error}", path.display())),
        }
    }

    if new_clients.is_empty() && !errors.is_empty() {
        return Err(errors.join("; "));
    }

    let mut settings = load_settings()?;
    let mut added = 0;

    for client in new_clients {
        if settings
            .clients
            .iter()
            .any(|existing| same_path(&existing.path, &client.path))
        {
            continue;
        }

        settings.clients.push(client);
        added += 1;
    }

    settings.watcher_enabled = true;
    settings.startup_enabled = true;
    refresh_client_metadata(&mut settings.clients);
    sort_clients(&mut settings.clients);
    save_settings(&settings)?;

    if added > 0 {
        record_log(
            LogLevel::Info,
            format!("Added {added} World of Warcraft client path(s)."),
        )?;
    } else {
        record_log(
            LogLevel::Warn,
            "Selected WoW path(s) were already configured.".to_string(),
        )?;
    }

    if !errors.is_empty() {
        record_log(
            LogLevel::Warn,
            format!(
                "Some selected WoW path(s) were ignored: {}",
                errors.join("; ")
            ),
        )?;
    }

    view_from_settings(settings)
}

pub fn remove_client(id: &str) -> Result<AppView, String> {
    let mut settings = load_settings()?;
    let before = settings.clients.len();
    settings.clients.retain(|client| client.id != id);
    save_settings(&settings)?;

    if settings.clients.len() != before {
        record_log(LogLevel::Info, "Removed WoW client path.".to_string())?;
    }

    view_from_settings(settings)
}

pub fn set_automation_enabled(enabled: bool) -> Result<AppView, String> {
    let mut settings = load_settings()?;
    settings.watcher_enabled = enabled;
    settings.startup_enabled = enabled;
    save_settings(&settings)?;
    record_log(
        LogLevel::Info,
        format!(
            "Brick automation {}.",
            if enabled { "enabled" } else { "paused" }
        ),
    )?;
    view_from_settings(settings)
}

pub fn record_log(level: LogLevel, message: String) -> Result<(), String> {
    let path = logs_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Failed to create {}: {error}", parent.display()))?;
    }

    let entry = LogEntry {
        at: now_stamp(),
        level,
        message,
    };
    let line = serde_json::to_string(&entry)
        .map_err(|error| format!("Failed to serialize log entry: {error}"))?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("Failed to open {}: {error}", path.display()))?;
    writeln!(file, "{line}").map_err(|error| format!("Failed to write {}: {error}", path.display()))
}

pub fn run_sync_with_lock(sync_lock: &Arc<Mutex<()>>) -> Result<SyncSummary, String> {
    let _guard = sync_lock
        .lock()
        .map_err(|_| "Brick sync lock was poisoned.".to_string())?;
    run_sync()
}

pub fn spawn_watcher(sync_lock: Arc<Mutex<()>>) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(SYNC_INTERVAL_SECS));

        if let Err(error) = run_sync_if_enabled(&sync_lock) {
            let _ = record_log(LogLevel::Error, error);
        }
    });
}

fn run_sync_if_enabled(sync_lock: &Arc<Mutex<()>>) -> Result<(), String> {
    let settings = load_settings()?;
    if !settings.watcher_enabled {
        return Ok(());
    }

    run_sync_with_lock(sync_lock).map(|_| ())
}

fn run_sync() -> Result<SyncSummary, String> {
    let mut settings = load_settings()?;
    maybe_auto_detect_clients(&mut settings)?;
    refresh_client_metadata(&mut settings.clients);

    let checked_at = now_stamp();
    if settings.clients.is_empty() {
        let summary = SyncSummary {
            version: None,
            checked_at,
            installed: 0,
            skipped: 0,
            message: "No WoW install configured yet.".to_string(),
        };
        record_log(LogLevel::Warn, summary.message.clone())?;
        return Ok(summary);
    }

    let manifest = match fetch_verified_manifest() {
        Ok(manifest) => manifest,
        Err(error) if error == FEED_UNAVAILABLE_MESSAGE => {
            let summary = SyncSummary {
                version: None,
                checked_at,
                installed: 0,
                skipped: settings.clients.len(),
                message: error,
            };
            record_log(LogLevel::Warn, summary.message.clone())?;
            return Ok(summary);
        }
        Err(error) => return Err(error),
    };
    validate_manifest(&manifest)?;
    let package = if settings
        .clients
        .iter()
        .any(|client| client_needs_install(client, &manifest))
    {
        Some(fetch_verified_package(&manifest)?)
    } else {
        None
    };

    let mut installed = 0;
    let mut skipped = 0;
    let mut errors = Vec::new();

    for client in &mut settings.clients {
        if !client_supported_by_manifest(client, &manifest) {
            skipped += 1;
            continue;
        }

        if !client_needs_install(client, &manifest) {
            skipped += 1;
            client.last_sync_at = Some(checked_at.clone());
            continue;
        }

        let Some(package) = package.as_deref() else {
            errors.push(format!(
                "{}: package download was skipped unexpectedly",
                client.path
            ));
            continue;
        };

        match install_package_for_client(client, &manifest, package) {
            Ok(()) => {
                installed += 1;
                client.last_installed_version = Some(manifest.version.clone());
                client.last_installed_sha256 = Some(manifest.artifact.sha256.clone());
                client.last_sync_at = Some(checked_at.clone());
            }
            Err(error) => {
                errors.push(format!("{}: {error}", client.path));
            }
        }
    }

    save_settings(&settings)?;

    let mut message = if installed > 0 {
        format!(
            "Installed {} on {installed} client(s).",
            manifest.version.trim()
        )
    } else {
        format!("{} is already installed.", manifest.version.trim())
    };

    if !errors.is_empty() {
        let joined = errors.join("; ");
        message = format!("{message} {joined}");
        record_log(LogLevel::Error, message.clone())?;
    } else {
        record_log(LogLevel::Info, message.clone())?;
    }

    Ok(SyncSummary {
        version: Some(manifest.version),
        checked_at,
        installed,
        skipped,
        message,
    })
}

fn view_from_settings(settings: Settings) -> Result<AppView, String> {
    Ok(AppView {
        setup_required: settings.clients.is_empty(),
        settings,
        logs: read_logs()?,
    })
}

fn load_settings() -> Result<Settings, String> {
    let path = settings_path()?;
    if !path.exists() {
        return Ok(Settings::default());
    }

    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    let mut settings: Settings = serde_json::from_str(&contents)
        .map_err(|error| format!("Failed to parse {}: {error}", path.display()))?;

    if settings.schema == 0 {
        settings.schema = 1;
    }

    Ok(settings)
}

fn save_settings(settings: &Settings) -> Result<(), String> {
    let path = settings_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Failed to create {}: {error}", parent.display()))?;
    }

    let json = serde_json::to_string_pretty(settings)
        .map_err(|error| format!("Failed to serialize settings: {error}"))?;
    fs::write(&path, json).map_err(|error| format!("Failed to write {}: {error}", path.display()))
}

fn read_logs() -> Result<Vec<LogEntry>, String> {
    let path = logs_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }

    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    let mut logs = Vec::new();

    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        if let Ok(entry) = serde_json::from_str::<LogEntry>(line) {
            logs.push(entry);
        }
    }

    if logs.len() > 80 {
        logs.drain(0..logs.len() - 80);
    }

    Ok(logs)
}

fn settings_path() -> Result<PathBuf, String> {
    Ok(config_dir()?.join(SETTINGS_FILE))
}

fn logs_path() -> Result<PathBuf, String> {
    Ok(config_dir()?.join(LOG_FILE))
}

fn config_dir() -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = env::var_os("APPDATA").or_else(|| env::var_os("LOCALAPPDATA")) {
            return Ok(PathBuf::from(appdata).join(APP_ID));
        }
        return Err("APPDATA/LOCALAPPDATA is not set.".to_string());
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(config_home).join(APP_ID));
        }
        if let Some(home) = env::var_os("HOME") {
            return Ok(PathBuf::from(home).join(".config").join(APP_ID));
        }
        Err("HOME/XDG_CONFIG_HOME is not set.".to_string())
    }
}

fn maybe_auto_detect_clients(settings: &mut Settings) -> Result<(), String> {
    if !settings.clients.is_empty() {
        return Ok(());
    }

    let detected = discover_default_wow_clients();
    if detected.is_empty() {
        return Ok(());
    }

    settings.clients = detected;
    sort_clients(&mut settings.clients);
    save_settings(settings)?;
    record_log(
        LogLevel::Info,
        format!(
            "Detected {} World of Warcraft client path(s).",
            settings.clients.len()
        ),
    )
}

fn discover_default_wow_clients() -> Vec<WowClient> {
    let mut clients = Vec::new();
    let mut seen = HashSet::new();

    for root in candidate_wow_roots() {
        if let Ok(found) = resolve_wow_clients(&root) {
            for client in found {
                if seen.insert(client.path.clone()) {
                    clients.push(client);
                }
            }
        }
    }

    clients
}

fn candidate_wow_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Ok(program_files_x86) = env::var("ProgramFiles(x86)") {
        roots.push(PathBuf::from(program_files_x86).join("World of Warcraft"));
    }
    if let Ok(program_files) = env::var("ProgramFiles") {
        roots.push(PathBuf::from(program_files).join("World of Warcraft"));
    }
    if let Ok(home) = env::var("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join("Games/battlenet/drive_c/Program Files (x86)/World of Warcraft"));
        roots.push(home.join(".wine/drive_c/Program Files (x86)/World of Warcraft"));
        roots.push(home.join(
            ".local/share/lutris/runners/wine/drive_c/Program Files (x86)/World of Warcraft",
        ));
    }

    roots.push(PathBuf::from(r"C:\Program Files (x86)\World of Warcraft"));
    roots.push(PathBuf::from(r"C:\Program Files\World of Warcraft"));

    roots
}

fn resolve_wow_clients(selected: &Path) -> Result<Vec<WowClient>, String> {
    let selected = normalize_path(selected);
    let mut roots = Vec::new();
    roots.push(selected.clone());

    if selected.file_name().and_then(|name| name.to_str()) == Some("AddOns") {
        if let Some(interface_dir) = selected.parent() {
            if interface_dir.file_name().and_then(|name| name.to_str()) == Some("Interface") {
                if let Some(flavor_root) = interface_dir.parent() {
                    roots.push(flavor_root.to_path_buf());
                }
            }
        }
    }

    if selected.file_name().and_then(|name| name.to_str()) == Some("Interface") {
        if let Some(flavor_root) = selected.parent() {
            roots.push(flavor_root.to_path_buf());
        }
    }

    let mut clients = Vec::new();
    let mut seen = HashSet::new();

    for root in roots {
        if let Some(info) = flavor_info_from_path(&root) {
            let client = client_from_flavor_dir(info, &root);
            if seen.insert(client.path.clone()) {
                clients.push(client);
            }
            continue;
        }

        for info in FLAVOR_INFOS {
            let flavor_root = root.join(info.directory);
            if flavor_root.is_dir() {
                let client = client_from_flavor_dir(info, &flavor_root);
                if seen.insert(client.path.clone()) {
                    clients.push(client);
                }
            }
        }
    }

    sort_clients(&mut clients);

    if clients.is_empty() {
        return Err(
            "Could not find a supported WoW client folder. Select the World of Warcraft folder or a _retail_/_ptr_ client folder."
                .to_string(),
        );
    }

    Ok(clients)
}

fn client_from_flavor_dir(info: &FlavorInfo, flavor_root: &Path) -> WowClient {
    let path = normalize_path(flavor_root);
    let game_version = detect_game_version(&path, info.flavor);
    WowClient {
        id: stable_client_id(info.flavor, &path),
        flavor: info.flavor,
        path: path.to_string_lossy().to_string(),
        game_version,
        last_installed_version: None,
        last_installed_sha256: None,
        last_sync_at: None,
    }
}

fn stable_client_id(flavor: Flavor, path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(flavor.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(path.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    format!("{}-{}", flavor.as_str(), &hex::encode(digest)[..12])
}

fn flavor_info_from_path(path: &Path) -> Option<&'static FlavorInfo> {
    let name = path.file_name()?.to_str()?;
    FLAVOR_INFOS.iter().find(|info| info.directory == name)
}

fn sort_clients(clients: &mut [WowClient]) {
    clients.sort_by_key(|client| (client.flavor.rank(), client.path.clone()));
}

fn refresh_client_metadata(clients: &mut [WowClient]) -> bool {
    let mut changed = false;

    for client in clients {
        let game_version = detect_game_version(Path::new(&client.path), client.flavor);
        if client.game_version != game_version {
            client.game_version = game_version;
            changed = true;
        }
    }

    changed
}

fn detect_game_version(flavor_root: &Path, flavor: Flavor) -> Option<String> {
    let wow_root = flavor_root.parent()?;
    let build_versions = read_build_versions(wow_root)?;
    let mut products = Vec::new();

    if let Some(product) = read_product_flavor(flavor_root) {
        products.push(product);
    }
    products.extend(
        product_candidates(flavor)
            .iter()
            .map(|product| product.to_string()),
    );

    for product in products {
        if let Some(version) = build_versions
            .iter()
            .find_map(|(build_product, version)| (build_product == &product).then_some(version))
        {
            return Some(version.clone());
        }
    }

    None
}

fn read_product_flavor(flavor_root: &Path) -> Option<String> {
    let contents = fs::read_to_string(flavor_root.join(".flavor.info")).ok()?;
    contents
        .lines()
        .skip(1)
        .find_map(|line| line.split('|').next())
        .map(str::trim)
        .filter(|product| !product.is_empty())
        .map(str::to_string)
}

fn read_build_versions(wow_root: &Path) -> Option<Vec<(String, String)>> {
    let contents = fs::read_to_string(wow_root.join(".build.info")).ok()?;
    let mut lines = contents.lines();
    let headers = lines.next()?.split('|').collect::<Vec<_>>();
    let product_index = headers
        .iter()
        .position(|header| header.split('!').next() == Some("Product"))?;
    let version_index = headers
        .iter()
        .position(|header| header.split('!').next() == Some("Version"))?;
    let mut versions = Vec::new();

    for line in lines {
        let columns = line.split('|').collect::<Vec<_>>();
        let Some(product) = columns.get(product_index).map(|value| value.trim()) else {
            continue;
        };
        let Some(version) = columns.get(version_index).map(|value| value.trim()) else {
            continue;
        };
        if !product.is_empty() && !version.is_empty() {
            versions.push((product.to_string(), version.to_string()));
        }
    }

    Some(versions)
}

fn product_candidates(flavor: Flavor) -> &'static [&'static str] {
    match flavor {
        Flavor::Retail => &["wow"],
        Flavor::Ptr => &["wowt", "wow_ptr", "wowptr"],
        Flavor::Xptr => &["wowxptr", "wow_xptr", "wowt", "wow_ptr", "wowptr"],
        Flavor::Beta => &["wow_beta", "wowbeta"],
        Flavor::Classic => &["wow_classic", "wow_classic_ptr"],
        Flavor::ClassicEra => &["wow_classic_era", "wow_classic_era_ptr"],
        Flavor::ClassicPtr => &["wow_classic_ptr"],
        Flavor::ClassicBeta => &["wow_classic_beta"],
    }
}

fn short_game_version(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    let parts = value.split('.').collect::<Vec<_>>();
    if parts.len() >= 3 {
        Some(parts[..3].join("."))
    } else {
        Some(value.to_string())
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn same_path(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn fetch_verified_manifest() -> Result<AddonManifest, String> {
    let client = http_client()?;
    let manifest_response = client
        .get(cache_busted_url(FEED_URL)?)
        .send()
        .map_err(|error| format!("Failed to download addon manifest: {error}"))?;
    if manifest_response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(FEED_UNAVAILABLE_MESSAGE.to_string());
    }
    let manifest_bytes = manifest_response
        .error_for_status()
        .map_err(|error| format!("Addon manifest request failed: {error}"))?
        .bytes()
        .map_err(|error| format!("Failed to read addon manifest: {error}"))?
        .to_vec();

    let sig_response = client
        .get(cache_busted_url(FEED_SIG_URL)?)
        .send()
        .map_err(|error| format!("Failed to download addon manifest signature: {error}"))?;
    if sig_response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(FEED_UNAVAILABLE_MESSAGE.to_string());
    }
    let sig_bytes = sig_response
        .error_for_status()
        .map_err(|error| format!("Addon manifest signature request failed: {error}"))?
        .bytes()
        .map_err(|error| format!("Failed to read addon manifest signature: {error}"))?
        .to_vec();

    verify_manifest_signature(&manifest_bytes, &sig_bytes)?;
    serde_json::from_slice(&manifest_bytes)
        .map_err(|error| format!("Failed to parse signed addon manifest: {error}"))
}

fn fetch_verified_package(manifest: &AddonManifest) -> Result<Vec<u8>, String> {
    validate_github_release_url(&manifest.artifact.url)?;
    let package_url = cache_busted_url(&manifest.artifact.url)?;

    let package = http_client()?
        .get(package_url)
        .send()
        .map_err(|error| format!("Failed to download addon package: {error}"))?
        .error_for_status()
        .map_err(|error| format!("Addon package request failed: {error}"))?
        .bytes()
        .map_err(|error| format!("Failed to read addon package: {error}"))?
        .to_vec();

    if package.len() as u64 != manifest.artifact.size {
        return Err(format!(
            "Addon package size mismatch: expected {}, got {}.",
            manifest.artifact.size,
            package.len()
        ));
    }

    let actual_hash = sha256_hex(&package);
    if actual_hash != manifest.artifact.sha256 {
        return Err(format!(
            "Addon package SHA-256 mismatch: expected {}, got {actual_hash}.",
            manifest.artifact.sha256
        ));
    }

    Ok(package)
}

fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(APP_USER_AGENT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|error| format!("Failed to create HTTP client: {error}"))
}

fn cache_busted_url(value: &str) -> Result<String, String> {
    let mut url = Url::parse(value).map_err(|error| format!("Invalid download URL: {error}"))?;
    url.query_pairs_mut()
        .append_pair("brickCache", &Uuid::new_v4().to_string());
    Ok(url.to_string())
}

fn verify_manifest_signature(
    manifest_bytes: &[u8],
    signature_response: &[u8],
) -> Result<(), String> {
    if ADDON_PUBLIC_KEY_B64.is_empty() {
        return Err("Brick was built without BRICK_ADDON_PUBLIC_KEY_B64.".to_string());
    }

    let public_key_bytes = B64
        .decode(ADDON_PUBLIC_KEY_B64)
        .map_err(|error| format!("Invalid embedded addon public key: {error}"))?;
    let public_key: [u8; 32] = public_key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| "Embedded addon public key must be 32 bytes.".to_string())?;
    let verifying_key = VerifyingKey::from_bytes(&public_key)
        .map_err(|error| format!("Invalid embedded addon public key: {error}"))?;

    let signature_text = String::from_utf8_lossy(signature_response);
    let signature_bytes = B64
        .decode(signature_text.trim())
        .map_err(|error| format!("Invalid addon manifest signature encoding: {error}"))?;
    let signature: [u8; 64] = signature_bytes
        .as_slice()
        .try_into()
        .map_err(|_| "Addon manifest signature must be 64 bytes.".to_string())?;
    let signature = Signature::from_bytes(&signature);

    verifying_key
        .verify(manifest_bytes, &signature)
        .map_err(|error| format!("Addon manifest signature verification failed: {error}"))
}

fn validate_manifest(manifest: &AddonManifest) -> Result<(), String> {
    if manifest.schema != 1 {
        return Err(format!(
            "Unsupported addon manifest schema {}.",
            manifest.schema
        ));
    }
    if manifest.package_id != PACKAGE_ID {
        return Err(format!(
            "Unexpected addon package id {}.",
            manifest.package_id
        ));
    }
    if manifest.version.trim().is_empty() {
        return Err("Addon manifest version is empty.".to_string());
    }
    if manifest.commit.trim().is_empty() || manifest.built_at.trim().is_empty() {
        return Err("Addon manifest is missing build metadata.".to_string());
    }
    if manifest.artifact.folders.is_empty() {
        return Err("Addon manifest does not list addon folders.".to_string());
    }
    if manifest.artifact.sha256.len() != 64
        || !manifest
            .artifact
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("Addon manifest contains an invalid SHA-256 hash.".to_string());
    }
    for folder in &manifest.artifact.folders {
        if !ALLOWED_FOLDERS.contains(&folder.as_str()) {
            return Err(format!(
                "Addon manifest contains unsupported folder {folder}."
            ));
        }
    }
    validate_github_release_url(&manifest.artifact.url)
}

fn validate_github_release_url(value: &str) -> Result<(), String> {
    let url = Url::parse(value).map_err(|error| format!("Invalid artifact URL: {error}"))?;
    if url.scheme() != "https" {
        return Err("Artifact URL must use HTTPS.".to_string());
    }
    if url.host_str() != Some("github.com") {
        return Err("Artifact URL must be hosted on github.com.".to_string());
    }

    let expected_prefix = format!("/{FEED_OWNER}/{FEED_REPO}/releases/download/{FEED_TAG}/");
    if !url.path().starts_with(&expected_prefix) {
        return Err("Artifact URL is outside the Brick addon feed release.".to_string());
    }

    Ok(())
}

fn installed_folders_present(client: &WowClient, manifest: &AddonManifest) -> bool {
    let addons_dir = addons_dir_for_client(client);
    manifest
        .artifact
        .folders
        .iter()
        .all(|folder| addons_dir.join(folder).is_dir())
}

fn client_supported_by_manifest(client: &WowClient, manifest: &AddonManifest) -> bool {
    manifest.artifact.flavors.is_empty() || manifest.artifact.flavors.contains(&client.flavor)
}

fn client_needs_install(client: &WowClient, manifest: &AddonManifest) -> bool {
    client_supported_by_manifest(client, manifest)
        && (client.last_installed_sha256.as_deref() != Some(manifest.artifact.sha256.as_str())
            || !installed_folders_present(client, manifest))
}

fn install_package_for_client(
    client: &WowClient,
    manifest: &AddonManifest,
    package: &[u8],
) -> Result<(), String> {
    let addons_dir = addons_dir_for_client(client);
    fs::create_dir_all(&addons_dir)
        .map_err(|error| format!("Failed to create {}: {error}", addons_dir.display()))?;

    let tx_id = format!("{}-{}", safe_version(&manifest.version), Uuid::new_v4());
    let staging_dir = addons_dir.join(".brick-staging").join(&tx_id);
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir)
            .map_err(|error| format!("Failed to clean {}: {error}", staging_dir.display()))?;
    }
    fs::create_dir_all(&staging_dir)
        .map_err(|error| format!("Failed to create {}: {error}", staging_dir.display()))?;

    extract_package(package, &staging_dir, &manifest.artifact.folders)?;

    for folder in &manifest.artifact.folders {
        if !staging_dir.join(folder).is_dir() {
            let _ = fs::remove_dir_all(&staging_dir);
            return Err(format!("Addon package did not contain folder {folder}."));
        }
    }

    for folder in &manifest.artifact.folders {
        let target = addons_dir.join(folder);
        if target.exists() {
            fs::remove_dir_all(&target)
                .map_err(|error| format!("Failed to delete {}: {error}", target.display()))?;
        }
    }

    for folder in &manifest.artifact.folders {
        let source = staging_dir.join(folder);
        let target = addons_dir.join(folder);
        move_dir(&source, &target).map_err(|error| {
            format!(
                "Failed to install {} to {}: {error}",
                source.display(),
                target.display()
            )
        })?;
    }

    let _ = fs::remove_dir_all(&staging_dir);
    prune_old_transactions(&addons_dir.join(".brick-staging"), 2);
    Ok(())
}

fn addons_dir_for_client(client: &WowClient) -> PathBuf {
    PathBuf::from(&client.path).join("Interface").join("AddOns")
}

fn extract_package(
    package: &[u8],
    staging_dir: &Path,
    allowed_folders: &[String],
) -> Result<(), String> {
    let mut archive = ZipArchive::new(Cursor::new(package))
        .map_err(|error| format!("Failed to open addon zip: {error}"))?;

    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|error| format!("Failed to read addon zip entry {index}: {error}"))?;
        let Some(enclosed_name) = file.enclosed_name() else {
            return Err(format!(
                "Addon zip entry {} has an unsafe path.",
                file.name()
            ));
        };

        let relative_path = validate_zip_path(&enclosed_name, allowed_folders)?;
        let output_path = staging_dir.join(relative_path);

        if file.is_dir() {
            fs::create_dir_all(&output_path).map_err(|error| {
                format!(
                    "Failed to create directory {}: {error}",
                    output_path.display()
                )
            })?;
        } else {
            if let Some(parent) = output_path.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    format!("Failed to create directory {}: {error}", parent.display())
                })?;
            }

            let mut output = fs::File::create(&output_path)
                .map_err(|error| format!("Failed to create {}: {error}", output_path.display()))?;
            io::copy(&mut file, &mut output)
                .map_err(|error| format!("Failed to extract {}: {error}", output_path.display()))?;
        }
    }

    Ok(())
}

fn validate_zip_path(path: &Path, allowed_folders: &[String]) -> Result<PathBuf, String> {
    let mut components = path.components();
    let first = components
        .next()
        .ok_or_else(|| "Addon zip contains an empty path.".to_string())?;

    let Component::Normal(first_name) = first else {
        return Err("Addon zip contains a non-normal top-level path.".to_string());
    };

    let first_name = first_name.to_string_lossy().to_string();
    if !allowed_folders.iter().any(|folder| folder == &first_name) {
        return Err(format!(
            "Addon zip contains unsupported top-level folder {first_name}."
        ));
    }

    for component in path.components() {
        let Component::Normal(part) = component else {
            return Err(format!(
                "Addon zip entry {} contains an unsafe path component.",
                path.display()
            ));
        };

        validate_zip_component(&part.to_string_lossy())?;
    }

    Ok(path.to_path_buf())
}

fn validate_zip_component(component: &str) -> Result<(), String> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains(':')
        || component.contains('\\')
        || component.contains('\0')
    {
        return Err(format!(
            "Addon zip contains unsafe path component {component}."
        ));
    }

    Ok(())
}

fn move_dir(source: &Path, target: &Path) -> io::Result<()> {
    if target.exists() {
        fs::remove_dir_all(target)?;
    }

    match fs::rename(source, target) {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_dir_all(source, target)?;
            fs::remove_dir_all(source)
        }
    }
}

fn copy_dir_all(source: &Path, target: &Path) -> io::Result<()> {
    fs::create_dir_all(target)?;

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let destination = target.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &destination)?;
        } else {
            fs::copy(entry.path(), destination)?;
        }
    }

    Ok(())
}

fn prune_old_transactions(parent: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };

    let mut dirs: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_type()
                .map(|file_type| file_type.is_dir())
                .unwrap_or(false)
        })
        .collect();
    dirs.sort_by_key(|entry| entry.file_name());

    let remove_count = dirs.len().saturating_sub(keep);
    for entry in dirs.into_iter().take(remove_count) {
        let _ = fs::remove_dir_all(entry.path());
    }
}

fn safe_version(version: &str) -> String {
    let cleaned: String = version
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
        .collect();

    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn now_stamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}
