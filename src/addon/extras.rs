//! Optional third-party troubleshooting addons.
//!
//! These addons belong to their authors and are not redistributed by Brick.
//! CI signs a small manifest that pins each addon's official download by URL,
//! size and SHA-256: the author's GitHub release zip, or the files of the
//! newest tag in the author's public CurseForge repository. Brick downloads
//! from those sources and installs only after the bytes match.

use std::{
    collections::{BTreeMap, HashMap},
    io::{Cursor, Write},
    path::Path,
};

use serde::Deserialize;
use url::Url;

use super::{
    addon_toc_matches_version, addons_dir_for_client, download_verified, ensure_sync_allowed,
    fetch_signed_manifest, install_folders, is_sha256_hex, record_log, validate_zip_path, Flavor,
    LogLevel, Settings, WowClient,
};

const FEED_URL: &str =
    "https://github.com/IsogiE/Brick-Releases/releases/download/addon-feed-v3/extra-addons-manifest.json";
const FEED_SIG_URL: &str =
    "https://github.com/IsogiE/Brick-Releases/releases/download/addon-feed-v3/extra-addons-manifest.json.sig";
const MANIFEST_KIND: &str = "brick-extra-addons";
const PACKAGE_MAX_BYTES: u64 = 16 * 1024 * 1024;
const SOURCE_MAX_FILES: usize = 2_000;
const CURSEFORGE_REPOS: &str = "https://repos.curseforge.com/wow/";
/// The BigWigs packager replaces this token with the tag when it builds a
/// release; files read straight from a tag still contain it.
const VERSION_TOKEN: &[u8] = b"@project-version@";

#[derive(Clone, Copy)]
enum Source {
    /// The packaged zip attached to the author's newest GitHub release.
    GitHubRelease { repo: &'static str },
    /// The files of the newest version tag in the author's CurseForge
    /// Subversion repository, which is readable without an API key.
    CurseForgeTag { project: &'static str },
}

/// Which panel an addon is listed under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Troubleshooting,
    BossMods,
}

impl Group {
    pub const ALL: [Group; 2] = [Group::Troubleshooting, Group::BossMods];

    pub fn title(self) -> &'static str {
        match self {
            Self::Troubleshooting => "Troubleshooting",
            Self::BossMods => "Boss mods",
        }
    }
}

pub struct ExtraAddon {
    pub id: &'static str,
    pub group: Group,
    pub name: &'static str,
    pub author: &'static str,
    pub project_url: &'static str,
    /// The main folder; its TOC version identifies the installed release.
    folder: &'static str,
    /// Extra top-level folders a package may ship, such as BigWigs' per-raid
    /// modules, which change every tier.
    folder_prefix: Option<&'static str>,
    source: Source,
    requires: &'static [&'static str],
    default_enabled: bool,
}

pub const CATALOG: &[ExtraAddon] = &[
    ExtraAddon {
        id: "buggrabber",
        group: Group::Troubleshooting,
        name: "BugGrabber",
        author: "Funkeh",
        project_url: "https://www.curseforge.com/wow/addons/bug-grabber",
        folder: "!BugGrabber",
        folder_prefix: None,
        source: Source::CurseForgeTag {
            project: "bug-grabber",
        },
        requires: &[],
        default_enabled: true,
    },
    ExtraAddon {
        id: "bugsack",
        group: Group::Troubleshooting,
        name: "BugSack",
        author: "Funkeh",
        project_url: "https://github.com/funkydude/BugSack",
        folder: "BugSack",
        folder_prefix: None,
        source: Source::GitHubRelease {
            repo: "funkydude/BugSack",
        },
        requires: &["buggrabber"],
        default_enabled: true,
    },
    ExtraAddon {
        id: "addon-profiler",
        group: Group::Troubleshooting,
        name: "AddOn Profiler",
        author: "Numy",
        project_url: "https://github.com/NumyAddon/AddonProfiler",
        folder: "!!AddonProfiler",
        folder_prefix: None,
        source: Source::GitHubRelease {
            repo: "NumyAddon/AddonProfiler",
        },
        requires: &[],
        default_enabled: true,
    },
    // Off by default: raiders who use another boss mod should not get a
    // second one.
    ExtraAddon {
        id: "bigwigs",
        group: Group::BossMods,
        name: "BigWigs",
        author: "BigWigsMods",
        project_url: "https://github.com/BigWigsMods/BigWigs",
        folder: "BigWigs",
        folder_prefix: Some("BigWigs_"),
        source: Source::GitHubRelease {
            repo: "BigWigsMods/BigWigs",
        },
        requires: &[],
        default_enabled: false,
    },
];

pub fn find(id: &str) -> Option<&'static ExtraAddon> {
    CATALOG.iter().find(|addon| addon.id == id)
}

pub fn is_enabled(settings: &Settings, id: &str) -> bool {
    let default = find(id).is_some_and(|addon| addon.default_enabled);
    settings.extra_addons.get(id).copied().unwrap_or(default)
}

/// Turning an addon on also turns its dependencies on.
pub(super) fn set_enabled(choices: &mut BTreeMap<String, bool>, addon: &ExtraAddon, enabled: bool) {
    choices.insert(addon.id.to_string(), enabled);
    if enabled {
        for id in addon.requires {
            choices.insert(id.to_string(), true);
        }
    }
    // Drop ids from older or newer catalogs so settings stay tidy.
    choices.retain(|id, _| find(id).is_some());
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema: u32,
    kind: String,
    addons: Vec<ManifestAddon>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestAddon {
    id: String,
    version: String,
    artifact: Artifact,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Artifact {
    /// Set for a release zip.
    #[serde(default)]
    url: String,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    size: u64,
    /// Set for an addon assembled from individual source files.
    #[serde(default)]
    files: Vec<SourceFile>,
    folders: Vec<String>,
    #[serde(default)]
    flavors: Vec<Flavor>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceFile {
    /// Install path, starting with the addon folder.
    path: String,
    url: String,
    sha256: String,
    size: u64,
}

/// Installs or updates every enabled extra addon. Failures are logged and
/// returned as `Ok`; only a pending account erasure stops the caller.
pub(super) fn sync(settings: &Settings) -> Result<(), String> {
    let wanted: Vec<_> = CATALOG
        .iter()
        .filter(|addon| is_enabled(settings, addon.id))
        .collect();
    if wanted.is_empty() || settings.clients.is_empty() {
        return Ok(());
    }

    ensure_sync_allowed()?;
    let manifest = match fetch_manifest() {
        Ok(Some(manifest)) => manifest,
        Ok(None) => return Ok(()),
        Err(error) => {
            ensure_sync_allowed()?;
            let _ = record_log(LogLevel::Warn, format!("Extra addons: {error}"));
            return Ok(());
        }
    };

    let mut errors = Vec::new();
    let mut installed = Vec::new();
    for addon in wanted {
        ensure_sync_allowed()?;
        let Some(entry) = manifest.addons.iter().find(|entry| entry.id == addon.id) else {
            continue;
        };
        if let Err(error) = validate_entry(addon, entry) {
            errors.push(format!("{}: {error}", addon.name));
            continue;
        }
        match sync_addon(&settings.clients, addon, entry) {
            Ok(0) => {}
            Ok(count) => installed.push(format!(
                "{} {} on {count} client(s)",
                addon.name,
                entry.version.trim()
            )),
            Err(error) => errors.push(error),
        }
    }

    ensure_sync_allowed()?;
    if !installed.is_empty() {
        let _ = record_log(
            LogLevel::Info,
            format!("Installed {}.", installed.join(", ")),
        );
    }
    if !errors.is_empty() {
        let _ = record_log(
            LogLevel::Error,
            format!("Extra addons: {}", errors.join("; ")),
        );
    }
    Ok(())
}

fn fetch_manifest() -> Result<Option<Manifest>, String> {
    let Some(bytes) = fetch_signed_manifest(FEED_URL, FEED_SIG_URL, "extra addon manifest")? else {
        return Ok(None);
    };
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Failed to parse signed extra addon manifest: {error}"))?;
    if manifest.schema != 1 || manifest.kind != MANIFEST_KIND {
        return Err(format!(
            "Unsupported extra addon manifest {} v{}.",
            manifest.kind, manifest.schema
        ));
    }
    Ok(Some(manifest))
}

/// Returns how many clients were updated.
fn sync_addon(
    clients: &[WowClient],
    addon: &ExtraAddon,
    entry: &ManifestAddon,
) -> Result<usize, String> {
    let targets: Vec<_> = clients
        .iter()
        .filter(|client| needs_install(client, entry))
        .collect();
    if targets.is_empty() {
        return Ok(0);
    }

    let package = match addon.source {
        Source::GitHubRelease { .. } => download_verified(
            &entry.artifact.url,
            entry.artifact.size,
            &entry.artifact.sha256,
            PACKAGE_MAX_BYTES,
            &format!("{} package", addon.name),
            true,
        ),
        Source::CurseForgeTag { .. } => download_source_package(entry),
    }
    .map_err(|error| format!("{}: {error}", addon.name))?;

    let mut installed = 0;
    let mut errors = Vec::new();
    for client in targets {
        ensure_sync_allowed()?;
        match install_folders(
            &addons_dir_for_client(client),
            &entry.version,
            &entry.artifact.folders,
            &package,
        ) {
            Ok(()) => installed += 1,
            Err(error) => errors.push(format!("{} on {}: {error}", addon.name, client.path)),
        }
    }
    if errors.is_empty() {
        Ok(installed)
    } else {
        Err(errors.join("; "))
    }
}

/// A copy from another addon manager at the same version is left alone, so
/// Brick and that manager never take turns replacing each other's files.
fn needs_install(client: &WowClient, entry: &ManifestAddon) -> bool {
    let supported =
        entry.artifact.flavors.is_empty() || entry.artifact.flavors.contains(&client.flavor);
    supported
        && !entry.artifact.folders.iter().all(|folder| {
            addon_toc_matches_version(&addons_dir_for_client(client), folder, &entry.version)
        })
}

fn validate_entry(addon: &ExtraAddon, entry: &ManifestAddon) -> Result<(), String> {
    let version = entry.version.trim();
    if version.is_empty()
        || version != entry.version
        || version.len() > 64
        || !version
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '+'))
    {
        return Err(format!("invalid version {:?}", entry.version));
    }
    validate_folders(addon, &entry.artifact.folders)?;
    match addon.source {
        Source::GitHubRelease { repo } => {
            if !entry.artifact.files.is_empty() {
                return Err("release entries cannot list source files".into());
            }
            if !is_sha256_hex(&entry.artifact.sha256) {
                return Err("invalid SHA-256 hash".into());
            }
            crate::download::validate_size(entry.artifact.size, PACKAGE_MAX_BYTES, "package")?;
            validate_release_url(&entry.artifact.url, repo)
        }
        Source::CurseForgeTag { project } => {
            if !entry.artifact.url.is_empty() {
                return Err("source entries cannot point at a package".into());
            }
            validate_source_files(addon.folder, project, &entry.version, &entry.artifact.files)
        }
    }
}

/// The main folder must be present, and any other folder must carry the
/// addon's prefix, so a package can never replace another addon.
fn validate_folders(addon: &ExtraAddon, folders: &[String]) -> Result<(), String> {
    if !folders.iter().any(|folder| folder == addon.folder) {
        return Err(format!("manifest folders do not include {}", addon.folder));
    }
    let mut seen = std::collections::HashSet::new();
    for folder in folders {
        let allowed = folder == addon.folder
            || addon.folder_prefix.is_some_and(|prefix| {
                folder.len() > prefix.len()
                    && folder.starts_with(prefix)
                    && folder
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            });
        if !allowed || !seen.insert(folder) {
            return Err(format!(
                "manifest folder {folder} is not part of {}",
                addon.name
            ));
        }
    }
    Ok(())
}

/// Every file must come from the pinned tag of the author's repository and
/// install inside the addon's own folder.
fn validate_source_files(
    folder: &str,
    project: &str,
    version: &str,
    files: &[SourceFile],
) -> Result<(), String> {
    if files.is_empty() || files.len() > SOURCE_MAX_FILES {
        return Err(format!("source must list 1 to {SOURCE_MAX_FILES} files"));
    }
    let toc = format!("{folder}/{folder}.toc");
    if !files.iter().any(|file| file.path == toc) {
        return Err(format!("source does not include {toc}"));
    }
    let base = Url::parse(&format!("{CURSEFORGE_REPOS}{project}/tags/{version}/"))
        .map_err(|error| format!("invalid source URL: {error}"))?;
    let folder_list = [folder.to_string()];
    let mut total = 0_u64;
    for file in files {
        validate_zip_path(Path::new(&file.path), &folder_list)?;
        let relative = file
            .path
            .strip_prefix(folder)
            .and_then(|rest| rest.strip_prefix('/'))
            .filter(|rest| !rest.is_empty())
            .ok_or_else(|| format!("source path {} is outside {folder}", file.path))?;
        let expected = base
            .join(relative)
            .map_err(|error| format!("invalid source path {}: {error}", file.path))?;
        let actual =
            Url::parse(&file.url).map_err(|error| format!("invalid source URL: {error}"))?;
        if actual != expected || actual.query().is_some() || actual.fragment().is_some() {
            return Err(format!(
                "source URL for {} is outside the pinned tag",
                file.path
            ));
        }
        if !is_sha256_hex(&file.sha256) {
            return Err(format!("invalid SHA-256 hash for {}", file.path));
        }
        if file.size > PACKAGE_MAX_BYTES {
            return Err(format!("{} is too large", file.path));
        }
        total += file.size;
    }
    if total > PACKAGE_MAX_BYTES {
        return Err("source files exceed the package size limit".into());
    }
    Ok(())
}

/// Downloads each pinned file and packs them into a zip so installation uses
/// the same staged, validated replacement as release packages. The version
/// token is filled in after each file's hash has been verified.
fn download_source_package(entry: &ManifestAddon) -> Result<Vec<u8>, String> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for file in &entry.artifact.files {
        ensure_sync_allowed()?;
        let mut bytes = if file.size == 0 {
            if !is_empty_sha256(&file.sha256) {
                return Err(format!("{} SHA-256 mismatch.", file.path));
            }
            Vec::new()
        } else {
            download_verified(
                &file.url,
                file.size,
                &file.sha256,
                PACKAGE_MAX_BYTES,
                &file.path,
                false,
            )?
        };
        if is_text_source(&file.path) {
            bytes = replace_all(&bytes, VERSION_TOKEN, entry.version.as_bytes());
        }
        writer
            .start_file(file.path.as_str(), options)
            .and_then(|()| writer.write_all(&bytes).map_err(Into::into))
            .map_err(|error| format!("Failed to stage {}: {error}", file.path))?;
    }
    writer
        .finish()
        .map(Cursor::into_inner)
        .map_err(|error| format!("Failed to stage source files: {error}"))
}

fn is_empty_sha256(value: &str) -> bool {
    value.eq_ignore_ascii_case("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
}

fn is_text_source(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".toc", ".lua", ".xml"]
        .iter()
        .any(|extension| lower.ends_with(extension))
}

fn replace_all(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut rest = haystack;
    while let Some(index) = rest
        .windows(needle.len())
        .position(|window| window == needle)
    {
        out.extend_from_slice(&rest[..index]);
        out.extend_from_slice(replacement);
        rest = &rest[index + needle.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// Only the author's own release downloads are accepted.
fn validate_release_url(value: &str, repo: &str) -> Result<(), String> {
    let url = Url::parse(value).map_err(|error| format!("invalid URL: {error}"))?;
    if url.scheme() != "https" || url.host_str() != Some("github.com") {
        return Err("package URL must be an https://github.com release asset".into());
    }
    let expected_prefix = format!("/{repo}/releases/download/");
    let path = url.path();
    let rest = path
        .strip_prefix(&expected_prefix)
        .ok_or_else(|| format!("package URL is outside {repo} releases"))?;
    let parts: Vec<_> = rest.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|part| part.is_empty() || *part == "..")
        || !parts[1].to_ascii_lowercase().ends_with(".zip")
    {
        return Err("package URL must point at a release zip".into());
    }
    Ok(())
}

/// Installed version per extra addon on the first client that has it, for the
/// settings panel.
pub fn installed_versions(clients: &[WowClient]) -> HashMap<String, String> {
    let mut versions = HashMap::new();
    for addon in CATALOG {
        for client in clients {
            if let Some(version) = installed_version(&addons_dir_for_client(client), addon.folder) {
                versions.insert(addon.id.to_string(), version);
                break;
            }
        }
    }
    versions
}

fn installed_version(addons_dir: &std::path::Path, folder: &str) -> Option<String> {
    let contents =
        std::fs::read_to_string(addons_dir.join(folder).join(format!("{folder}.toc"))).ok()?;
    contents
        .trim_start_matches('\u{feff}')
        .lines()
        .find_map(|line| {
            let (key, value) = line.trim().strip_prefix("##")?.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case("Version")
                .then(|| value.trim().to_string())
        })
        .filter(|version| !version.is_empty() && !version.starts_with('@'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write, path::PathBuf};
    use zip::{write::SimpleFileOptions, ZipWriter};

    fn bugsack() -> &'static ExtraAddon {
        find("bugsack").unwrap()
    }

    fn entry(url: &str, folders: &[&str]) -> ManifestAddon {
        ManifestAddon {
            id: "bugsack".into(),
            version: "v12.1.9".into(),
            artifact: Artifact {
                url: url.into(),
                sha256: "a".repeat(64),
                size: 71193,
                files: vec![],
                folders: folders.iter().map(|folder| folder.to_string()).collect(),
                flavors: vec![],
            },
        }
    }

    const BUGSACK_URL: &str =
        "https://github.com/funkydude/BugSack/releases/download/v12.1.9/BugSack-v12.1.9.zip";

    #[test]
    fn accepts_only_the_authors_release_zip_for_its_own_folder() {
        assert!(validate_entry(bugsack(), &entry(BUGSACK_URL, &["BugSack"])).is_ok());

        for url in [
            "http://github.com/funkydude/BugSack/releases/download/v12.1.9/BugSack-v12.1.9.zip",
            "https://evil.example/funkydude/BugSack/releases/download/v12.1.9/BugSack.zip",
            "https://github.com/someone/BugSack/releases/download/v12.1.9/BugSack.zip",
            "https://github.com/funkydude/BugSack/archive/refs/tags/v12.1.9.zip",
            "https://github.com/funkydude/BugSack/releases/download/v12.1.9/release.json",
            "https://github.com/IsogiE/Brick-Releases/releases/download/addon-feed-v3/BugSack.zip",
        ] {
            assert!(
                validate_entry(bugsack(), &entry(url, &["BugSack"])).is_err(),
                "{url}"
            );
        }
        for folders in [
            &["AdvanceRaidTools"][..],
            &["BugSack", "!BugGrabber"][..],
            &[][..],
        ] {
            assert!(validate_entry(bugsack(), &entry(BUGSACK_URL, folders)).is_err());
        }

        let mut bad = entry(BUGSACK_URL, &["BugSack"]);
        bad.version = "v1/../../x".into();
        assert!(validate_entry(bugsack(), &bad).is_err());
        let mut bad = entry(BUGSACK_URL, &["BugSack"]);
        bad.artifact.size = PACKAGE_MAX_BYTES + 1;
        assert!(validate_entry(bugsack(), &bad).is_err());
    }

    fn grabber_entry(files: Vec<SourceFile>) -> ManifestAddon {
        ManifestAddon {
            id: "buggrabber".into(),
            version: "v12.1.1".into(),
            artifact: Artifact {
                url: String::new(),
                sha256: String::new(),
                size: 0,
                files,
                folders: vec!["!BugGrabber".into()],
                flavors: vec![],
            },
        }
    }

    fn source_file(path: &str, url: &str) -> SourceFile {
        SourceFile {
            path: path.into(),
            url: url.into(),
            sha256: "b".repeat(64),
            size: 10,
        }
    }

    const TAG: &str = "https://repos.curseforge.com/wow/bug-grabber/tags/v12.1.1/";

    #[test]
    fn source_files_must_come_from_the_pinned_tag_and_stay_in_the_folder() {
        let grabber = find("buggrabber").unwrap();
        let good = || {
            vec![
                source_file(
                    "!BugGrabber/!BugGrabber.toc",
                    &format!("{TAG}!BugGrabber.toc"),
                ),
                source_file(
                    "!BugGrabber/BugGrabber.lua",
                    &format!("{TAG}BugGrabber.lua"),
                ),
            ]
        };
        assert!(validate_entry(grabber, &grabber_entry(good())).is_ok());

        let bad_files = [
            (
                "!BugGrabber/BugGrabber.lua",
                "https://repos.curseforge.com/wow/bug-grabber/trunk/BugGrabber.lua",
            ),
            (
                "!BugGrabber/BugGrabber.lua",
                "https://repos.curseforge.com/wow/bug-grabber/tags/v12.1.0/BugGrabber.lua",
            ),
            (
                "!BugGrabber/BugGrabber.lua",
                "https://repos.curseforge.com/wow/other/tags/v12.1.1/BugGrabber.lua",
            ),
            (
                "!BugGrabber/BugGrabber.lua",
                "http://repos.curseforge.com/wow/bug-grabber/tags/v12.1.1/BugGrabber.lua",
            ),
            (
                "!BugGrabber/BugGrabber.lua",
                "https://evil.example/wow/bug-grabber/tags/v12.1.1/BugGrabber.lua",
            ),
            (
                "!BugGrabber/BugGrabber.lua",
                "https://repos.curseforge.com/wow/bug-grabber/tags/v12.1.1/Other.lua",
            ),
            (
                "!BugGrabber/BugGrabber.lua",
                "https://repos.curseforge.com/wow/bug-grabber/tags/v12.1.1/BugGrabber.lua?p=1",
            ),
            (
                "AdvanceRaidTools/x.lua",
                "https://repos.curseforge.com/wow/bug-grabber/tags/v12.1.1/x.lua",
            ),
            (
                "!BugGrabber/../x.lua",
                "https://repos.curseforge.com/wow/bug-grabber/tags/v12.1.1/../x.lua",
            ),
            (
                "!BugGrabber",
                "https://repos.curseforge.com/wow/bug-grabber/tags/v12.1.1/",
            ),
        ];
        for (path, url) in bad_files {
            let mut files = good();
            files.push(source_file(path, url));
            assert!(
                validate_entry(grabber, &grabber_entry(files)).is_err(),
                "{path} {url}"
            );
        }

        assert!(validate_entry(grabber, &grabber_entry(good()[1..].to_vec())).is_err());
        let mut with_zip = grabber_entry(good());
        with_zip.artifact.url = BUGSACK_URL.into();
        assert!(validate_entry(grabber, &with_zip).is_err());
        let mut oversized = good();
        oversized[1].size = PACKAGE_MAX_BYTES;
        assert!(validate_entry(grabber, &grabber_entry(oversized)).is_err());
        // A release entry cannot smuggle in loose files either.
        let mut mixed = entry(BUGSACK_URL, &["BugSack"]);
        mixed.artifact.files = good();
        assert!(validate_entry(bugsack(), &mixed).is_err());
    }

    #[test]
    fn every_group_has_addons_and_bigwigs_is_not_a_troubleshooting_addon() {
        for group in Group::ALL {
            assert!(CATALOG.iter().any(|addon| addon.group == group));
        }
        assert_eq!(find("bigwigs").unwrap().group, Group::BossMods);
        for id in ["buggrabber", "bugsack", "addon-profiler"] {
            assert_eq!(find(id).unwrap().group, Group::Troubleshooting);
        }
    }

    #[test]
    fn bigwigs_may_ship_its_own_module_folders_but_nothing_else() {
        let bigwigs = find("bigwigs").unwrap();
        let url =
            "https://github.com/BigWigsMods/BigWigs/releases/download/v427.8/BigWigs-v427.8.zip";
        let mut ok = entry(url, &["BigWigs", "BigWigs_Core", "BigWigs_TheVoidspire"]);
        ok.id = "bigwigs".into();
        assert!(validate_entry(bigwigs, &ok).is_ok());
        for folders in [
            &["BigWigs_Core"][..],
            &["BigWigs", "BigWigs_"][..],
            &["BigWigs", "LittleWigs"][..],
            &["BigWigs", "BigWigs_Core", "BigWigs_Core"][..],
            &["BigWigs", "BigWigs_../x"][..],
        ] {
            assert!(
                validate_entry(bigwigs, &entry(url, folders)).is_err(),
                "{folders:?}"
            );
        }
        // Other addons stay limited to their single folder.
        assert!(validate_entry(bugsack(), &entry(BUGSACK_URL, &["BugSack", "BugSack_x"])).is_err());
    }

    #[test]
    fn version_token_is_filled_only_in_text_sources() {
        assert_eq!(
            replace_all(
                b"## Version: @project-version@\n@project-version@",
                VERSION_TOKEN,
                b"v1"
            ),
            b"## Version: v1\nv1"
        );
        assert_eq!(replace_all(b"none", VERSION_TOKEN, b"v1"), b"none");
        assert!(is_text_source("!BugGrabber/!BugGrabber.toc"));
        assert!(is_text_source("!BugGrabber/BugGrabber.lua"));
        assert!(!is_text_source("!BugGrabber/icon.tga"));
    }

    #[test]
    fn extras_are_on_by_default_and_turning_one_on_restores_its_dependencies() {
        let mut settings = Settings::default();
        let legacy: Settings =
            serde_json::from_str(r#"{"schema":1,"startupEnabled":true,"clients":[]}"#).unwrap();
        for addon in CATALOG {
            let expected = addon.id != "bigwigs";
            assert_eq!(is_enabled(&settings, addon.id), expected, "{}", addon.id);
            assert_eq!(is_enabled(&legacy, addon.id), expected, "{}", addon.id);
        }
        set_enabled(&mut settings.extra_addons, find("bigwigs").unwrap(), true);
        assert!(is_enabled(&settings, "bigwigs"));

        let choices = &mut settings.extra_addons;
        choices.insert("retired-addon".into(), true);
        set_enabled(choices, find("buggrabber").unwrap(), false);
        set_enabled(choices, bugsack(), false);
        assert!(!is_enabled(&settings, "buggrabber"));
        assert!(!is_enabled(&settings, "bugsack"));
        assert!(!settings.extra_addons.contains_key("retired-addon"));
        set_enabled(&mut settings.extra_addons, bugsack(), true);
        assert!(is_enabled(&settings, "buggrabber"));
        assert!(is_enabled(&settings, "bugsack"));
    }

    #[test]
    fn manifest_kind_keeps_the_art_feed_and_extras_feed_apart() {
        let art = br#"{"schema":1,"packageId":"AdvanceRaidTools","version":"v1","commit":"c","builtAt":"t","artifact":{"url":"u","sha256":"s","size":1,"folders":[]}}"#;
        assert!(serde_json::from_slice::<Manifest>(art).is_err());
        let extras = br#"{"schema":1,"kind":"brick-extra-addons","addons":[]}"#;
        assert!(serde_json::from_slice::<super::super::AddonManifest>(extras).is_err());
    }

    fn client(root: &std::path::Path) -> WowClient {
        super::super::client_from_flavor_dir(&super::super::FLAVOR_INFOS[0], root)
    }

    fn write_toc(addons: &std::path::Path, folder: &str, version: &str) {
        fs::create_dir_all(addons.join(folder)).unwrap();
        fs::write(
            addons.join(folder).join(format!("{folder}.toc")),
            format!("## Interface: 120105\n## Version: {version}\n"),
        )
        .unwrap();
    }

    #[test]
    fn same_version_from_another_manager_is_left_alone() {
        let root = std::env::temp_dir().join(format!("brick-extras-{}", uuid::Uuid::new_v4()));
        let _cleanup = super::super::StagingCleanup(&root);
        let retail = root.join("_retail_");
        let client = client(&retail);
        let addons = addons_dir_for_client(&client);
        let entry = entry(BUGSACK_URL, &["BugSack"]);

        assert!(needs_install(&client, &entry));
        write_toc(&addons, "BugSack", "v12.1.8");
        assert!(needs_install(&client, &entry));
        assert_eq!(
            installed_versions(std::slice::from_ref(&client)).get("bugsack"),
            Some(&"v12.1.8".to_string())
        );
        write_toc(&addons, "BugSack", "v12.1.9");
        assert!(!needs_install(&client, &entry));

        let mut classic_only = entry;
        classic_only.artifact.flavors = vec![Flavor::ClassicEra];
        write_toc(&addons, "BugSack", "v12.1.8");
        assert!(!needs_install(&client, &classic_only));
    }

    #[test]
    fn installs_a_release_zip_without_touching_other_addons() {
        let root = std::env::temp_dir().join(format!("brick-extras-{}", uuid::Uuid::new_v4()));
        let _cleanup = super::super::StagingCleanup(&root);
        let client = client(&root.join("_retail_"));
        let addons: PathBuf = addons_dir_for_client(&client);
        write_toc(&addons, "AdvanceRaidTools", "v1.7.11");
        write_toc(&addons, "!!AddonProfiler", "v1.4.37");
        fs::write(addons.join("!!AddonProfiler").join("stale.lua"), "old").unwrap();

        let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        writer
            .start_file("!!AddonProfiler/!!AddonProfiler.toc", options)
            .unwrap();
        writer
            .write_all(b"## Title: AddonProfiler\n## Version: v1.4.38\n")
            .unwrap();
        writer
            .start_file("!!AddonProfiler/AddonProfiler.lua", options)
            .unwrap();
        writer.write_all(b"-- profiler").unwrap();
        let zip = writer.finish().unwrap().into_inner();

        install_folders(&addons, "v1.4.38", &["!!AddonProfiler".to_string()], &zip).unwrap();
        assert!(addon_toc_matches_version(
            &addons,
            "!!AddonProfiler",
            "v1.4.38"
        ));
        assert!(!addons.join("!!AddonProfiler").join("stale.lua").exists());
        assert!(addon_toc_matches_version(
            &addons,
            "AdvanceRaidTools",
            "v1.7.11"
        ));

        // A zip that smuggles in another folder is rejected before any change.
        let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file("AdvanceRaidTools/AdvanceRaidTools.toc", options)
            .unwrap();
        writer.write_all(b"## Version: evil\n").unwrap();
        let zip = writer.finish().unwrap().into_inner();
        assert!(
            install_folders(&addons, "v1.4.39", &["!!AddonProfiler".to_string()], &zip).is_err()
        );
        assert!(addon_toc_matches_version(
            &addons,
            "AdvanceRaidTools",
            "v1.7.11"
        ));
        assert!(addon_toc_matches_version(
            &addons,
            "!!AddonProfiler",
            "v1.4.38"
        ));
    }
}
