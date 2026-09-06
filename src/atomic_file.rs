use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use uuid::Uuid;

/// Replace a complete file without ever truncating the previous contents.
pub(crate) fn write(path: &Path, contents: &[u8]) -> io::Result<()> {
    write_with(path, |file| {
        file.write_all(contents)?;
        // File is unbuffered. sync_all also surfaces delayed disk-full errors.
        file.sync_all()
    })
}

fn write_with(
    path: &Path,
    write_and_sync: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    let directory = File::open(parent)?;

    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "File name is missing"))?;
    let mut temporary_name = name.to_os_string();
    temporary_name.push(format!(".{}.tmp", Uuid::new_v4()));
    let temporary_path = parent.join(temporary_name);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // This helper also writes Discord credentials. Protect the temporary
        // file from creation, as well as the final file after replacement.
        options.mode(0o600);
    }
    let file = options.open(&temporary_path)?;
    let _cleanup = TemporaryFile(temporary_path.clone());
    // Drop the open handle before cleanup or replacement, including on errors.
    let mut file = file;
    write_and_sync(&mut file)?;
    drop(file);

    // Both paths share a directory/filesystem. Rust's rename replaces existing
    // files on Windows too; never remove the destination or fall back to copying.
    fs::rename(&temporary_path, path)?;
    #[cfg(unix)]
    directory.sync_all()?;
    Ok(())
}

struct TemporaryFile(PathBuf);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("brick-atomic-test-{}", Uuid::new_v4()));
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
    fn creates_and_replaces_complete_files_ignoring_abandoned_temporary_files() {
        let root = TestDirectory::new();
        let path = root.0.join("settings.json");
        let abandoned = root.0.join("settings.json.old.tmp");
        fs::write(&abandoned, b"incomplete").unwrap();
        write(&path, b"original").unwrap();
        write(&path, b"replacement").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        assert_eq!(fs::read(abandoned).unwrap(), b"incomplete");
        assert_eq!(fs::read_dir(&root.0).unwrap().count(), 2);
    }

    #[test]
    fn write_and_flush_failures_preserve_original_and_remove_temporary_file() {
        let root = TestDirectory::new();
        let path = root.0.join("settings.json");
        write(&path, b"original").unwrap();
        for partial in [b"".as_slice(), b"partial", b"complete but flush failed"] {
            let error = write_with(&path, |file| {
                file.write_all(partial)?;
                assert_eq!(fs::read(&path)?, b"original");
                Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "injected disk full",
                ))
            })
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::StorageFull);
            assert_eq!(fs::read(&path).unwrap(), b"original");
            assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
        }
        write(&path, b"retry succeeded").unwrap();
        assert_eq!(fs::read(path).unwrap(), b"retry succeeded");
    }

    #[test]
    fn failed_replacement_preserves_destination_and_cleans_temporary_file() {
        let root = TestDirectory::new();
        let path = root.0.join("settings.json");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep"), b"original").unwrap();
        assert!(write(&path, b"replacement").is_err());
        assert_eq!(fs::read(path.join("keep")).unwrap(), b"original");
        assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
    }

    #[test]
    fn process_exit_before_replacement_preserves_original_and_allows_retry() {
        const TEST_PATH: &str = "BRICK_ATOMIC_EXIT_TEST_PATH";
        if let Some(path) = std::env::var_os(TEST_PATH) {
            let _ = write_with(Path::new(&path), |file| {
                file.write_all(b"partial replacement")?;
                file.sync_all()?;
                // Exit without running destructors, like an interrupted save.
                std::process::exit(17);
            });
            panic!("Child did not reach the interrupted-save point");
        }
        let root = TestDirectory::new();
        let path = root.0.join("settings.json");
        write(&path, b"original").unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "atomic_file::tests::process_exit_before_replacement_preserves_original_and_allows_retry"])
            .env(TEST_PATH, &path)
            .output().unwrap();
        assert_eq!(output.status.code(), Some(17));
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(fs::read_dir(&root.0).unwrap().count(), 2);
        write(&path, b"retry succeeded").unwrap();
        assert_eq!(fs::read(path).unwrap(), b"retry succeeded");
    }

    #[cfg(unix)]
    #[test]
    fn private_permissions_survive_replacement() {
        use std::os::unix::fs::PermissionsExt;
        let root = TestDirectory::new();
        let path = root.0.join("discord-auth.dat");
        for payload in [b"first".as_slice(), b"second"] {
            write(&path, payload).unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn locked_destination_is_preserved_and_can_be_retried() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = TestDirectory::new();
        let path = root.0.join("settings.json");
        write(&path, b"original").unwrap();
        let locked = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        assert!(write(&path, b"replacement").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
        drop(locked);
        write(&path, b"replacement").unwrap();
        assert_eq!(fs::read(path).unwrap(), b"replacement");
    }
}
