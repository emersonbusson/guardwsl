//! Short exclusion between cleanup and heavy builds to preserve in-use artifacts.

use crate::fsutil::{effective_uid, flock_file};
use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct MaintenanceLock {
    file: File,
}

impl MaintenanceLock {
    pub fn acquire(timeout: Duration) -> Result<Self> {
        Self::acquire_at(&default_lock_path(), timeout)
    }

    pub fn acquire_shared(timeout: Duration) -> Result<Self> {
        Self::acquire_shared_at(&default_lock_path(), timeout)
    }

    pub fn try_acquire() -> Result<Option<Self>> {
        Self::try_acquire_at(&default_lock_path())
    }

    pub fn acquire_at(path: &Path, timeout: Duration) -> Result<Self> {
        Self::acquire_at_operation(path, timeout, libc::LOCK_EX)
    }

    pub fn acquire_shared_at(path: &Path, timeout: Duration) -> Result<Self> {
        Self::acquire_at_operation(path, timeout, libc::LOCK_SH)
    }

    fn acquire_at_operation(
        path: &Path,
        timeout: Duration,
        operation: libc::c_int,
    ) -> Result<Self> {
        let started = Instant::now();
        loop {
            if let Some(lock) = Self::try_acquire_at_operation(path, operation)? {
                return Ok(lock);
            }
            if started.elapsed() >= timeout {
                bail!("another maintenance task or heavy build is still active after {timeout:?}")
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn try_acquire_at(path: &Path) -> Result<Option<Self>> {
        Self::try_acquire_at_operation(path, libc::LOCK_EX)
    }

    fn try_acquire_at_operation(path: &Path, operation: libc::c_int) -> Result<Option<Self>> {
        let parent = path.parent().context("lock has no parent directory")?;
        validate_parent(parent)?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let file = options.open(path)?;
        validate_file(&file, path)?;
        let result = flock_file(&file, operation | libc::LOCK_NB);
        if result != 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.raw_os_error(),
                Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
            ) {
                return Ok(None);
            }
            return Err(error.into());
        }
        let named = std::fs::symlink_metadata(path)?;
        let opened = file.metadata()?;
        if named.dev() != opened.dev() || named.ino() != opened.ino() || named.nlink() != 1 {
            flock_file(&file, libc::LOCK_UN);
            bail!("maintenance lock was replaced")
        }
        Ok(Some(Self { file }))
    }
}

fn default_lock_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", effective_uid())));
    runtime.join("guardwsl-maintenance.lock")
}

impl Drop for MaintenanceLock {
    fn drop(&mut self) {
        flock_file(&self.file, libc::LOCK_UN);
    }
}

fn validate_parent(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != effective_uid()
        || metadata.permissions().mode() & 0o022 != 0
    {
        bail!("unsafe lock directory: {}", path.display())
    }
    Ok(())
}

fn validate_file(file: &File, path: &Path) -> Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != effective_uid()
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o077 != 0
    {
        bail!("unsafe lock file: {}", path.display())
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn serializes_and_releases_on_drop() {
        let directory = tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("maintenance.lock");
        let first = MaintenanceLock::acquire_at(&path, Duration::from_millis(20)).unwrap();
        assert!(MaintenanceLock::try_acquire_at(&path).unwrap().is_none());
        drop(first);
        assert!(MaintenanceLock::try_acquire_at(&path).unwrap().is_some());
    }

    #[test]
    fn shared_build_holders_coexist_but_exclude_cleanup() {
        let directory = tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("maintenance.lock");
        let first = MaintenanceLock::acquire_shared_at(&path, Duration::from_millis(20)).unwrap();
        let second = MaintenanceLock::acquire_shared_at(&path, Duration::from_millis(20)).unwrap();
        assert!(MaintenanceLock::try_acquire_at(&path).unwrap().is_none());
        drop(first);
        assert!(MaintenanceLock::try_acquire_at(&path).unwrap().is_none());
        drop(second);
        assert!(MaintenanceLock::try_acquire_at(&path).unwrap().is_some());
    }

    #[test]
    fn symlink_lock_is_rejected_without_touching_target() {
        let directory = tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, b"safe").unwrap();
        let path = directory.path().join("maintenance.lock");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(MaintenanceLock::try_acquire_at(&path).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"safe");
    }
}
