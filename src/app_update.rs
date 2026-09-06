use std::{
    cmp::Ordering,
    convert::TryInto,
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::LazyLock,
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use crate::download;

const APP_UPDATE_OWNER: &str = "IsogiE";
const APP_UPDATE_REPO: &str = "Brick-Releases";
const APP_UPDATE_TAG: &str = "app-feed";
const APP_PACKAGE_ID: &str = "Brick";
const APP_MANIFEST_URL: &str =
    "https://github.com/IsogiE/Brick-Releases/releases/download/app-feed/app-manifest.json";
const APP_MANIFEST_SIG_URL: &str =
    "https://github.com/IsogiE/Brick-Releases/releases/download/app-feed/app-manifest.json.sig";
const APP_UPDATE_USER_AGENT: &str = concat!(
    "Brick/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/IsogiE/Brick-Releases)"
);
const FEED_UNAVAILABLE_MESSAGE: &str = "No signed Brick app update feed is available yet.";
const INSTALLER_MAX_BYTES: u64 = 256 * 1024 * 1024;

const APP_UPDATE_PUBLIC_KEY_B64: &str = match option_env!("BRICK_ADDON_PUBLIC_KEY_B64") {
    Some(value) => value,
    None => "",
};

static HTTP_CLIENT: LazyLock<Result<reqwest::blocking::Client, String>> = LazyLock::new(|| {
    reqwest::blocking::Client::builder()
        .user_agent(APP_UPDATE_USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|error| format!("Failed to create HTTP client: {error}"))
});

#[derive(Debug, Clone)]
pub struct PreparedAppUpdate {
    pub version: String,
    pub installer_path: PathBuf,
    replacement_path: Option<PathBuf>,
    kind: UpdateKind,
    sha256: String,
    size: u64,
}

#[derive(Debug, Clone)]
pub struct AvailableAppUpdate {
    pub version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateKind {
    WindowsNsis,
    LinuxAppImage,
}

struct PreparedPackage {
    installer_path: PathBuf,
    replacement_path: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppUpdateManifest {
    schema: u32,
    package_id: String,
    version: String,
    commit: String,
    built_at: String,
    artifacts: Vec<AppUpdateArtifact>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppUpdateArtifact {
    os: String,
    arch: String,
    kind: String,
    file_name: String,
    url: String,
    sha256: String,
    size: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Vec<VersionPart>,
}

#[derive(Debug, PartialEq, Eq)]
enum VersionPart {
    Numeric(u64),
    Text(String),
}

pub fn check_available_update() -> Result<Option<AvailableAppUpdate>, String> {
    // The running version confirms installation succeeded. This also retries
    // cleanup when Windows still held the installer open during the restart.
    cleanup_installer_cache();

    let Some((manifest, _artifact_index)) = find_available_update()? else {
        return Ok(None);
    };

    Ok(Some(AvailableAppUpdate {
        version: manifest.version,
    }))
}

pub fn prepare_available_update() -> Result<Option<PreparedAppUpdate>, String> {
    let Some((manifest, artifact_index)) = find_available_update()? else {
        return Ok(None);
    };
    let artifact = &manifest.artifacts[artifact_index];
    let package = fetch_verified_artifact(artifact)?;
    let prepared_package = write_installer(&manifest.version, artifact, &package)?;

    Ok(Some(PreparedAppUpdate {
        version: manifest.version,
        sha256: artifact.sha256.clone(),
        size: artifact.size,
        installer_path: prepared_package.installer_path,
        replacement_path: prepared_package.replacement_path,
        kind: target_update_kind()
            .ok_or_else(|| "Brick app updates are not supported for this install.".to_string())?,
    }))
}

fn find_available_update() -> Result<Option<(AppUpdateManifest, usize)>, String> {
    let Some(update_kind) = target_update_kind() else {
        return Ok(None);
    };
    if APP_UPDATE_PUBLIC_KEY_B64.is_empty() {
        return Ok(None);
    }

    let manifest = match fetch_verified_manifest() {
        Ok(manifest) => manifest,
        Err(error) if error == FEED_UNAVAILABLE_MESSAGE => return Ok(None),
        Err(error) => return Err(error),
    };

    validate_manifest(&manifest)?;
    if !is_newer_version(&manifest.version, env!("CARGO_PKG_VERSION"))? {
        return Ok(None);
    }

    let artifact_index = select_artifact_index(&manifest, &update_kind).ok_or_else(|| {
        format!(
            "Brick {} is available, but no supported installer was published.",
            manifest.version
        )
    })?;
    Ok(Some((manifest, artifact_index)))
}

fn select_artifact_index(manifest: &AppUpdateManifest, update_kind: &UpdateKind) -> Option<usize> {
    let arch = target_arch();

    manifest.artifacts.iter().position(|artifact| {
        artifact.os == update_kind.os()
            && artifact.arch == arch
            && artifact.kind == update_kind.artifact_kind()
            && update_kind.file_name_matches(&artifact.file_name)
    })
}

pub fn launch_installer(update: &PreparedAppUpdate) -> Result<(), String> {
    verify_prepared_installer(update)?;
    match update.kind {
        UpdateKind::WindowsNsis => launch_windows_nsis_installer(update),
        UpdateKind::LinuxAppImage => launch_linux_appimage(update),
    }
}

fn verify_prepared_installer(update: &PreparedAppUpdate) -> Result<(), String> {
    let metadata = fs::symlink_metadata(&update.installer_path)
        .map_err(|error| format!("Failed to inspect Brick installer: {error}"))?;
    if !metadata.is_file() || metadata.len() != update.size {
        return Err(
            "Brick installer changed after download. Please try updating again.".to_string(),
        );
    }
    let mut file = fs::File::open(&update.installer_path)
        .map_err(|error| format!("Failed to read Brick installer: {error}"))?
        .take(update.size.saturating_add(1));
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut size = 0_u64;
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("Failed to verify Brick installer: {error}"))?;
        if count == 0 {
            break;
        }
        size += count as u64;
        hash.update(&buffer[..count]);
    }
    if size != update.size || hex::encode(hash.finalize()) != update.sha256 {
        return Err(
            "Brick installer changed after download. Please try updating again.".to_string(),
        );
    }
    Ok(())
}

fn launch_windows_nsis_installer(update: &PreparedAppUpdate) -> Result<(), String> {
    if !cfg!(target_os = "windows") {
        return Err("Brick NSIS updates are only supported on Windows.".to_string());
    }
    let mut command = Command::new(&update.installer_path);
    command.arg("/S").arg("/R").arg("/NS");
    if let Some(install_dir) = current_user_windows_install_dir() {
        command.arg(format!("/D={}", install_dir.display()));
    }

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Failed to start Brick installer: {error}"))?;

    Ok(())
}

fn launch_linux_appimage(update: &PreparedAppUpdate) -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("Brick AppImage updates are only supported on Linux.".to_string());
    }

    let replacement_path = update
        .replacement_path
        .as_ref()
        .ok_or_else(|| "Brick AppImage update target was not prepared.".to_string())?;

    fs::rename(&update.installer_path, replacement_path).map_err(|error| {
        format!(
            "Failed to replace Brick AppImage {}: {error}",
            replacement_path.display()
        )
    })?;

    Command::new("sh")
        .arg("-c")
        .arg("sleep 0.8; exec \"$1\"")
        .arg("brick-restart")
        .arg(replacement_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Failed to restart Brick: {error}"))?;

    Ok(())
}

fn target_update_kind() -> Option<UpdateKind> {
    if cfg!(target_os = "windows") {
        Some(UpdateKind::WindowsNsis)
    } else if cfg!(target_os = "linux") && current_appimage_path().is_some() {
        Some(UpdateKind::LinuxAppImage)
    } else {
        None
    }
}

fn fetch_verified_manifest() -> Result<AppUpdateManifest, String> {
    let client = http_client()?;
    let manifest_response = client
        .get(cache_busted_url(APP_MANIFEST_URL)?)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(|error| format!("Failed to download Brick app manifest: {error}"))?;
    if manifest_response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(FEED_UNAVAILABLE_MESSAGE.to_string());
    }
    let manifest_bytes = download::read_response(
        manifest_response
            .error_for_status()
            .map_err(|error| format!("Brick app manifest request failed: {error}"))?,
        download::MANIFEST_MAX_BYTES,
        "Brick app manifest",
    )?;

    let sig_response = client
        .get(cache_busted_url(APP_MANIFEST_SIG_URL)?)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(|error| format!("Failed to download Brick app manifest signature: {error}"))?;
    if sig_response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(FEED_UNAVAILABLE_MESSAGE.to_string());
    }
    let sig_bytes = download::read_response(
        sig_response
            .error_for_status()
            .map_err(|error| format!("Brick app manifest signature request failed: {error}"))?,
        download::SIGNATURE_MAX_BYTES,
        "Brick app manifest signature",
    )?;

    verify_manifest_signature(&manifest_bytes, &sig_bytes)?;
    serde_json::from_slice(&manifest_bytes)
        .map_err(|error| format!("Failed to parse signed Brick app manifest: {error}"))
}

fn fetch_verified_artifact(artifact: &AppUpdateArtifact) -> Result<Vec<u8>, String> {
    validate_github_release_url(&artifact.url)?;
    download::validate_size(artifact.size, INSTALLER_MAX_BYTES, "Brick installer")?;
    let response = http_client()?
        .get(cache_busted_url(&artifact.url)?)
        .send()
        .map_err(|error| format!("Failed to download Brick installer: {error}"))?
        .error_for_status()
        .map_err(|error| format!("Brick installer request failed: {error}"))?;
    let package = download::read_exact_response(
        response,
        artifact.size,
        INSTALLER_MAX_BYTES,
        "Brick installer",
    )?;

    let actual_hash = sha256_hex(&package);
    if actual_hash != artifact.sha256 {
        return Err(format!(
            "Brick installer SHA-256 mismatch: expected {}, got {actual_hash}.",
            artifact.sha256
        ));
    }

    Ok(package)
}

fn verify_manifest_signature(
    manifest_bytes: &[u8],
    signature_response: &[u8],
) -> Result<(), String> {
    let public_key_bytes = B64
        .decode(APP_UPDATE_PUBLIC_KEY_B64)
        .map_err(|error| format!("Invalid embedded Brick app update public key: {error}"))?;
    let public_key: [u8; 32] = public_key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| "Embedded Brick app update public key must be 32 bytes.".to_string())?;
    let verifying_key = VerifyingKey::from_bytes(&public_key)
        .map_err(|error| format!("Invalid embedded Brick app update public key: {error}"))?;

    let signature_text = String::from_utf8_lossy(signature_response);
    let signature_bytes = B64
        .decode(signature_text.trim())
        .map_err(|error| format!("Invalid Brick app manifest signature encoding: {error}"))?;
    let signature: [u8; 64] = signature_bytes
        .as_slice()
        .try_into()
        .map_err(|_| "Brick app manifest signature must be 64 bytes.".to_string())?;
    let signature = Signature::from_bytes(&signature);

    verifying_key
        .verify(manifest_bytes, &signature)
        .map_err(|error| format!("Brick app manifest signature verification failed: {error}"))
}

fn validate_manifest(manifest: &AppUpdateManifest) -> Result<(), String> {
    if manifest.schema != 1 {
        return Err(format!(
            "Unsupported Brick app manifest schema {}.",
            manifest.schema
        ));
    }
    if manifest.package_id != APP_PACKAGE_ID {
        return Err(format!(
            "Unexpected Brick app package id {}.",
            manifest.package_id
        ));
    }
    parse_semver(&manifest.version)?;
    if manifest.commit.trim().is_empty() || manifest.built_at.trim().is_empty() {
        return Err("Brick app manifest is missing build metadata.".to_string());
    }
    if manifest.artifacts.is_empty() {
        return Err("Brick app manifest does not list installers.".to_string());
    }

    for artifact in &manifest.artifacts {
        if artifact.os.trim().is_empty()
            || artifact.arch.trim().is_empty()
            || artifact.kind.trim().is_empty()
            || artifact.file_name.trim().is_empty()
        {
            return Err("Brick app manifest contains an incomplete installer.".to_string());
        }
        download::validate_size(artifact.size, INSTALLER_MAX_BYTES, "Brick installer")?;
        if artifact.sha256.len() != 64
            || !artifact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("Brick app manifest contains an invalid SHA-256 hash.".to_string());
        }
        validate_github_release_url(&artifact.url)?;
    }

    Ok(())
}

#[cfg(test)]
fn select_artifact<'a>(
    manifest: &'a AppUpdateManifest,
    update_kind: &UpdateKind,
) -> Option<&'a AppUpdateArtifact> {
    select_artifact_index(manifest, update_kind).map(|index| &manifest.artifacts[index])
}

impl UpdateKind {
    fn os(&self) -> &'static str {
        match self {
            Self::WindowsNsis => "windows",
            Self::LinuxAppImage => "linux",
        }
    }

    fn artifact_kind(&self) -> &'static str {
        match self {
            Self::WindowsNsis => "nsis",
            Self::LinuxAppImage => "appimage",
        }
    }

    fn file_name_matches(&self, file_name: &str) -> bool {
        let file_name = file_name.to_ascii_lowercase();
        match self {
            Self::WindowsNsis => file_name.ends_with(".exe"),
            Self::LinuxAppImage => file_name.ends_with(".appimage"),
        }
    }
}

fn target_arch() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "unknown"
    }
}

fn is_newer_version(candidate: &str, current: &str) -> Result<bool, String> {
    Ok(compare_semver(&parse_semver(candidate)?, &parse_semver(current)?) == Ordering::Greater)
}

fn parse_semver(value: &str) -> Result<SemVer, String> {
    let value = value.trim().trim_start_matches('v');
    let (core, prerelease) = value.split_once('-').unwrap_or((value, ""));
    let mut parts = core.split('.');
    let major = parse_version_number(parts.next(), value)?;
    let minor = parse_version_number(parts.next(), value)?;
    let patch = parse_version_number(parts.next(), value)?;

    if parts.next().is_some() {
        return Err(format!("Invalid Brick app version {value}."));
    }

    let prerelease = if prerelease.is_empty() {
        Vec::new()
    } else {
        prerelease
            .split('.')
            .map(|part| {
                if part.is_empty() {
                    return Err(format!("Invalid Brick app version {value}."));
                }
                Ok(match part.parse::<u64>() {
                    Ok(number) => VersionPart::Numeric(number),
                    Err(_) => VersionPart::Text(part.to_ascii_lowercase()),
                })
            })
            .collect::<Result<Vec<_>, String>>()?
    };

    Ok(SemVer {
        major,
        minor,
        patch,
        prerelease,
    })
}

fn parse_version_number(part: Option<&str>, full: &str) -> Result<u64, String> {
    let Some(part) = part else {
        return Err(format!("Invalid Brick app version {full}."));
    };
    if part.is_empty() || (part.len() > 1 && part.starts_with('0')) {
        return Err(format!("Invalid Brick app version {full}."));
    }
    part.parse::<u64>()
        .map_err(|_| format!("Invalid Brick app version {full}."))
}

fn compare_semver(left: &SemVer, right: &SemVer) -> Ordering {
    left.major
        .cmp(&right.major)
        .then_with(|| left.minor.cmp(&right.minor))
        .then_with(|| left.patch.cmp(&right.patch))
        .then_with(|| compare_prerelease(&left.prerelease, &right.prerelease))
}

fn compare_prerelease(left: &[VersionPart], right: &[VersionPart]) -> Ordering {
    match (left.is_empty(), right.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }

    for (left, right) in left.iter().zip(right.iter()) {
        let ordering = match (left, right) {
            (VersionPart::Numeric(left), VersionPart::Numeric(right)) => left.cmp(right),
            (VersionPart::Numeric(_), VersionPart::Text(_)) => Ordering::Less,
            (VersionPart::Text(_), VersionPart::Numeric(_)) => Ordering::Greater,
            (VersionPart::Text(left), VersionPart::Text(right)) => left.cmp(right),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }

    left.len().cmp(&right.len())
}

fn write_installer(
    version: &str,
    artifact: &AppUpdateArtifact,
    package: &[u8],
) -> Result<PreparedPackage, String> {
    let (update_dir, replacement_path) = update_location(artifact)?;
    fs::create_dir_all(&update_dir).map_err(|error| {
        format!(
            "Failed to create Brick update folder {}: {error}",
            update_dir.display()
        )
    })?;

    let installer_path = update_dir.join(format!(
        "brick-{}-{}",
        safe_path_part(version),
        safe_path_part(&artifact.file_name)
    ));
    crate::atomic_file::write(&installer_path, package).map_err(|error| {
        format!(
            "Failed to write Brick installer {}: {error}",
            installer_path.display()
        )
    })?;

    mark_executable_if_needed(artifact, &installer_path)?;

    Ok(PreparedPackage {
        installer_path,
        replacement_path,
    })
}

fn update_location(artifact: &AppUpdateArtifact) -> Result<(PathBuf, Option<PathBuf>), String> {
    if artifact.kind == "appimage" {
        let replacement_path = current_appimage_path()
            .ok_or_else(|| "Brick AppImage updates require an AppImage install.".to_string())?;
        let update_dir = replacement_path
            .parent()
            .ok_or_else(|| "Brick AppImage path has no parent folder.".to_string())?
            .to_path_buf();
        return Ok((update_dir, Some(replacement_path)));
    }

    Ok((installer_cache_dir(), None))
}

fn installer_cache_dir() -> PathBuf {
    env::temp_dir().join("Brick").join("updates")
}

fn cleanup_installer_cache() {
    let Ok(current_version) = parse_semver(env!("CARGO_PKG_VERSION")) else {
        return;
    };
    let protected_paths: Vec<_> = [env::current_exe().ok(), current_appimage_path()]
        .into_iter()
        .flatten()
        .collect();
    cleanup_completed_installers(&installer_cache_dir(), &current_version, &protected_paths);
}

fn cleanup_completed_installers(
    directory: &Path,
    current_version: &SemVer,
    protected_paths: &[PathBuf],
) {
    // Only remove recognized, regular files in Brick's own cache. Do not follow
    // links or recurse into directories, including a redirected cache root.
    let Ok(metadata) = fs::symlink_metadata(directory) else {
        return;
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return;
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if protected_paths.contains(&path) {
            continue;
        }
        let name = entry.file_name();
        let Some(version) = name.to_str().and_then(cached_installer_version) else {
            continue;
        };
        // A newer download may be awaiting installation or a retry. Retain it
        // until that version (or a later one) is actually running.
        if compare_semver(&version, current_version) != Ordering::Greater {
            let _ = fs::remove_file(path);
        }
    }
    // A busy installer or an unrecognized entry simply keeps the folder alive.
    let _ = fs::remove_dir(directory);
}

fn cached_installer_version(file_name: &str) -> Option<SemVer> {
    let name = file_name.to_ascii_lowercase();
    let (version, artifact_suffix) = name.strip_prefix("brick-")?.rsplit_once("-brick")?;
    if !(artifact_suffix.ends_with(".exe")
        || artifact_suffix.ends_with(".msi")
        || artifact_suffix.ends_with(".appimage"))
    {
        return None;
    }
    parse_semver(version).ok()
}

#[cfg(target_os = "linux")]
fn mark_executable_if_needed(artifact: &AppUpdateArtifact, path: &PathBuf) -> Result<(), String> {
    if artifact.kind != "appimage" {
        return Ok(());
    }

    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .map_err(|error| format!("Failed to read Brick AppImage permissions: {error}"))?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .map_err(|error| format!("Failed to mark Brick AppImage executable: {error}"))
}

#[cfg(not(target_os = "linux"))]
fn mark_executable_if_needed(_artifact: &AppUpdateArtifact, _path: &PathBuf) -> Result<(), String> {
    Ok(())
}

fn current_appimage_path() -> Option<PathBuf> {
    let path = PathBuf::from(env::var_os("APPIMAGE")?);
    let file_name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    if !file_name.ends_with(".appimage") {
        return None;
    }

    Some(path)
}

#[cfg(target_os = "windows")]
fn current_user_windows_install_dir() -> Option<PathBuf> {
    env::var_os("LOCALAPPDATA").map(|path| PathBuf::from(path).join("Brick"))
}

#[cfg(not(target_os = "windows"))]
fn current_user_windows_install_dir() -> Option<PathBuf> {
    None
}

fn safe_path_part(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => ch,
            _ => '-',
        })
        .collect()
}

fn validate_github_release_url(value: &str) -> Result<(), String> {
    let url = Url::parse(value).map_err(|error| format!("Invalid Brick app URL: {error}"))?;
    if url.scheme() != "https" {
        return Err("Brick app URL must use HTTPS.".to_string());
    }
    if url.host_str() != Some("github.com") {
        return Err("Brick app URL must be hosted on github.com.".to_string());
    }

    let segments = url
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    if segments.len() < 6
        || segments[0] != APP_UPDATE_OWNER
        || segments[1] != APP_UPDATE_REPO
        || segments[2] != "releases"
        || segments[3] != "download"
        || segments[4] == APP_UPDATE_TAG
        || !segments[4].starts_with('v')
    {
        return Err("Brick app URL must point to a versioned Brick release asset.".to_string());
    }

    Ok(())
}

fn http_client() -> Result<reqwest::blocking::Client, String> {
    match &*HTTP_CLIENT {
        Ok(client) => Ok(client.clone()),
        Err(error) => Err(error.clone()),
    }
}

fn cache_busted_url(value: &str) -> Result<String, String> {
    let mut url = Url::parse(value).map_err(|error| format!("Invalid download URL: {error}"))?;
    url.query_pairs_mut()
        .append_pair("brickCache", &Uuid::new_v4().to_string());
    Ok(url.to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::{
        cleanup_completed_installers, is_newer_version, parse_semver, select_artifact,
        validate_github_release_url, AppUpdateArtifact, AppUpdateManifest, UpdateKind,
    };

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("brick-cache-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cleanup_removes_completed_installers_but_preserves_pending_and_unrelated_files() {
        let root = TestDirectory::new();
        let removed = [
            "brick-0.2.1-Brick.msi",
            "brick-v0.3.5-brick_0.3.5_x64-setup.exe",
            "brick-0.3.6-beta.1-Brick.exe",
            "brick-0.3.6-brick_0.3.6_x64-setup.exe",
        ];
        let retained = [
            "brick-0.3.7-beta.1-brick_0.3.7-beta.1_x64-setup.exe",
            "brick-0.3.10-brick_0.3.10_x64-setup.exe",
            "notes.txt",
            "other.exe",
            "brick-unknown-Brick.exe",
            "brick-0.3.5-Brick.txt",
        ];
        for name in removed.iter().chain(retained.iter()) {
            fs::write(root.0.join(name), b"keep until installed").unwrap();
        }
        let nested = root.0.join("brick-0.3.5-Brick.exe");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("keep.txt"), b"keep").unwrap();

        cleanup_completed_installers(&root.0, &parse_semver("0.3.6").unwrap(), &[]);

        for name in removed {
            assert!(!root.0.join(name).exists(), "{name}");
        }
        for name in retained {
            assert_eq!(
                fs::read(root.0.join(name)).unwrap(),
                b"keep until installed"
            );
        }
        assert_eq!(fs::read(nested.join("keep.txt")).unwrap(), b"keep");
    }

    #[test]
    fn cleanup_waits_for_the_downloaded_version_to_run_and_removes_the_empty_cache() {
        let root = TestDirectory::new();
        let installer = root.0.join("brick-0.3.7-Brick.exe");
        fs::write(&installer, b"pending").unwrap();
        cleanup_completed_installers(&root.0, &parse_semver("0.3.7-beta.1").unwrap(), &[]);
        assert!(installer.exists());

        cleanup_completed_installers(&root.0, &parse_semver("0.3.7").unwrap(), &[]);
        assert!(!root.0.exists());
        cleanup_completed_installers(&root.0, &parse_semver("0.3.7").unwrap(), &[]);
    }

    #[test]
    fn cleanup_preserves_the_running_image_even_in_the_legacy_cache() {
        let root = TestDirectory::new();
        let image = root.0.join("brick-0.3.6-brick_0.3.6_x86_64.AppImage");
        fs::write(&image, b"running image").unwrap();
        cleanup_completed_installers(
            &root.0,
            &parse_semver("0.3.6").unwrap(),
            std::slice::from_ref(&image),
        );
        assert_eq!(fs::read(image).unwrap(), b"running image");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_does_not_follow_cache_or_file_symlinks() {
        use std::os::unix::fs::symlink;

        let root = TestDirectory::new();
        let outside = TestDirectory::new();
        let name = "brick-0.3.5-Brick.exe";
        let target = outside.0.join(name);
        fs::write(&target, b"outside cache").unwrap();
        symlink(&target, root.0.join(name)).unwrap();
        cleanup_completed_installers(&root.0, &parse_semver("0.3.6").unwrap(), &[]);
        assert!(root.0.join(name).is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"outside cache");

        let linked_cache = root.0.join("updates");
        symlink(&outside.0, &linked_cache).unwrap();
        cleanup_completed_installers(&linked_cache, &parse_semver("0.3.6").unwrap(), &[]);
        assert!(linked_cache.is_symlink());
        assert_eq!(fs::read(target).unwrap(), b"outside cache");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn cleanup_retries_an_installer_after_windows_releases_it() {
        use std::os::windows::fs::OpenOptionsExt;

        let root = TestDirectory::new();
        let installer = root.0.join("brick-0.3.6-Brick.exe");
        fs::write(&installer, b"busy installer").unwrap();
        let open_installer = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&installer)
            .unwrap();
        cleanup_completed_installers(&root.0, &parse_semver("0.3.6").unwrap(), &[]);
        assert!(installer.exists());

        drop(open_installer);
        cleanup_completed_installers(&root.0, &parse_semver("0.3.6").unwrap(), &[]);
        assert!(!root.0.exists());
    }

    #[test]
    fn compares_release_versions() {
        assert!(is_newer_version("0.1.2", "0.1.1").unwrap());
        assert!(!is_newer_version("0.1.1", "0.1.1").unwrap());
        assert!(!is_newer_version("0.1.0", "0.1.1").unwrap());
        assert!(is_newer_version("0.2.0", "0.1.9").unwrap());
    }

    #[test]
    fn handles_prerelease_precedence() {
        assert!(is_newer_version("0.1.2", "0.1.2-beta.1").unwrap());
        assert!(is_newer_version("0.1.2-beta.2", "0.1.2-beta.1").unwrap());
        assert!(!is_newer_version("0.1.2-beta.1", "0.1.2").unwrap());
    }

    #[test]
    fn accepts_only_versioned_brick_release_urls() {
        assert!(validate_github_release_url(
            "https://github.com/IsogiE/Brick-Releases/releases/download/v0.1.2/Brick.exe"
        )
        .is_ok());
        assert!(validate_github_release_url(
            "https://github.com/IsogiE/Brick-Releases/releases/download/app-feed/Brick.exe"
        )
        .is_err());
        assert!(validate_github_release_url(
            "https://github.com/SomeoneElse/Brick-Releases/releases/download/v0.1.2/Brick.exe"
        )
        .is_err());
    }

    #[test]
    fn selects_windows_nsis_artifact() {
        let manifest = test_manifest(vec![
            artifact("linux", "x86_64", "appimage", "brick_0.2.0_x86_64.AppImage"),
            artifact("windows", "x86_64", "nsis", "Brick_0.2.0_x64-setup.exe"),
        ]);

        let selected = select_artifact(&manifest, &UpdateKind::WindowsNsis).unwrap();
        assert_eq!(selected.file_name, "Brick_0.2.0_x64-setup.exe");
    }

    #[test]
    fn selects_linux_appimage_artifact() {
        let manifest = test_manifest(vec![
            artifact("windows", "x86_64", "nsis", "Brick_0.2.0_x64-setup.exe"),
            artifact("linux", "x86_64", "appimage", "brick_0.2.0_x86_64.AppImage"),
        ]);

        let selected = select_artifact(&manifest, &UpdateKind::LinuxAppImage).unwrap();
        assert_eq!(selected.file_name, "brick_0.2.0_x86_64.AppImage");
    }

    #[test]
    fn accepts_appimage_release_urls() {
        assert!(validate_github_release_url(
            "https://github.com/IsogiE/Brick-Releases/releases/download/v0.2.0/brick_0.2.0_x86_64.AppImage"
        )
        .is_ok());
    }

    fn test_manifest(artifacts: Vec<AppUpdateArtifact>) -> AppUpdateManifest {
        AppUpdateManifest {
            schema: 1,
            package_id: "Brick".to_string(),
            version: "0.2.0".to_string(),
            commit: "abc123".to_string(),
            built_at: "2026-09-04T00:00:00Z".to_string(),
            artifacts,
        }
    }

    fn artifact(os: &str, arch: &str, kind: &str, file_name: &str) -> AppUpdateArtifact {
        AppUpdateArtifact {
            os: os.to_string(),
            arch: arch.to_string(),
            kind: kind.to_string(),
            file_name: file_name.to_string(),
            url: format!(
                "https://github.com/IsogiE/Brick-Releases/releases/download/v0.2.0/{file_name}"
            ),
            sha256: "a".repeat(64),
            size: 1,
        }
    }

    #[test]
    fn rechecks_installer_integrity_before_launch() {
        let root = TestDirectory::new();
        let path = root.0.join("fixture-installer");
        let package = b"verified fixture";
        fs::write(&path, package).unwrap();
        let update = super::PreparedAppUpdate {
            version: "1.0.0".to_string(),
            installer_path: path.clone(),
            replacement_path: None,
            kind: super::UpdateKind::WindowsNsis,
            sha256: super::sha256_hex(package),
            size: package.len() as u64,
        };
        assert!(super::verify_prepared_installer(&update).is_ok());
        fs::write(&path, b"tampered fixture").unwrap();
        assert!(super::verify_prepared_installer(&update).is_err());
        fs::write(&path, b"truncated").unwrap();
        assert!(super::verify_prepared_installer(&update).is_err());
        fs::remove_file(&path).unwrap();
        assert!(super::verify_prepared_installer(&update).is_err());
    }
}
