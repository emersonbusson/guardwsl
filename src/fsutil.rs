//! Small private-file operations used by configuration and auditing.

use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[must_use]
pub(crate) fn effective_uid() -> libc::uid_t {
    // SAFETY: geteuid has no arguments, preconditions, or failure mode.
    unsafe { libc::geteuid() }
}

pub(crate) fn flock_file(file: &File, operation: libc::c_int) -> libc::c_int {
    // SAFETY: File owns a valid descriptor for the duration of this call.
    unsafe { libc::flock(file.as_raw_fd(), operation) }
}

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("could not create {}", path.display()))?;
    #[cfg(target_os = "linux")]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("invalid private directory: {}", path.display())
    }
    #[cfg(target_os = "linux")]
    if metadata.uid() != effective_uid() || metadata.permissions().mode() & 0o022 != 0 {
        bail!("unsafe owner or mode at {}", path.display())
    }
    Ok(())
}

pub fn read_private(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let mut file = options
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))?;
    validate_private_file(&file, path)?;
    let size = file.metadata()?.len();
    if size > max_bytes {
        bail!("{} exceeds the {max_bytes}-byte limit", path.display())
    }
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("file has no parent directory")?;
    ensure_private_dir(parent)?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("invalid file name")?;
    let mut temporary = None;
    for _ in 0..100 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{name}.tmp-{}-{sequence}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(target_os = "linux")]
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        match options.open(&candidate) {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let (temporary_path, mut file) =
        temporary.context("could not create a unique temporary file")?;
    let result = (|| -> Result<()> {
        validate_private_file(&file, &temporary_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary_path, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    result
}

pub fn append_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("file has no parent directory")?;
    ensure_private_dir(parent)?;
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(target_os = "linux")]
    options
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let mut file = options.open(path)?;
    validate_private_file(&file, path)?;
    let locked = flock_file(&file, libc::LOCK_EX);
    if locked != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to serialize append");
    }
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_data()?;
        Ok(())
    })();
    flock_file(&file, libc::LOCK_UN);
    result
}

fn validate_private_file(file: &File, path: &Path) -> Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("private file is not regular: {}", path.display())
    }
    #[cfg(target_os = "linux")]
    if metadata.uid() != effective_uid()
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o077 != 0
    {
        bail!("unsafe owner, mode, or link count at {}", path.display())
    }
    Ok(())
}

pub fn default_state_dir() -> Result<PathBuf> {
    let root = match std::env::var_os("XDG_STATE_HOME") {
        Some(value) => {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                bail!("XDG_STATE_HOME must be absolute")
            }
            path
        }
        None => std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .context("an absolute HOME or XDG_STATE_HOME is required")?
            .join(".local/state"),
    };
    Ok(root.join("guardwsl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn atomic_write_replaces_a_dangling_symlink_without_following_it() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("status.json");
        let victim = directory.path().join("victim");
        std::os::unix::fs::symlink(&victim, &path).unwrap();
        atomic_write_private(&path, b"safe").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"safe");
        assert!(!victim.exists());
    }

    #[test]
    fn append_is_complete_and_private() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("audit.jsonl");
        append_private(&path, b"one\n").unwrap();
        append_private(&path, b"two\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one\ntwo\n");
        #[cfg(target_os = "linux")]
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
