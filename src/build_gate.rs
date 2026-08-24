//! Local build gate based only on flock(2).
//!
//! The kernel releases the lock when the process exits. There are no leases,
//! TTLs, brokers, sockets, or distributed state that can become stale.

use crate::fsutil::{effective_uid, flock_file};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io;
#[cfg(target_os = "linux")]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateState {
    Idle,
    HeavyBuildActive,
}

#[derive(Debug, Clone)]
pub struct BuildGate {
    path: PathBuf,
}

impl BuildGate {
    pub fn for_current_user() -> Result<Self> {
        let runtime = runtime_directory()?;
        Ok(Self::at(runtime.join("guardwsl-build.lock")))
    }

    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Heavy builds use an exclusive lock with an explicit wait deadline.
    pub fn acquire_heavy(&self, timeout: Duration) -> Result<GateGuard> {
        self.acquire_path(&self.path, libc::LOCK_EX, timeout)
            .context("timed out waiting for the previous build")
    }

    pub fn state(&self) -> Result<GateState> {
        if let Some(guard) = self.try_acquire_path(&self.path, libc::LOCK_EX)? {
            drop(guard);
            return Ok(GateState::Idle);
        }
        Ok(GateState::HeavyBuildActive)
    }

    fn try_acquire_path(&self, path: &Path, operation: libc::c_int) -> Result<Option<GateGuard>> {
        let file = open_lock_file(path)?;
        let result = flock_file(&file, operation | libc::LOCK_NB);
        if result == 0 {
            verify_named_inode(&file, path)?;
            return Ok(Some(GateGuard { file }));
        }
        let error = io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
        ) {
            Ok(None)
        } else {
            Err(error).with_context(|| format!("could not lock {}", path.display()))
        }
    }

    fn acquire_path(
        &self,
        path: &Path,
        operation: libc::c_int,
        timeout: Duration,
    ) -> Result<GateGuard> {
        let started = Instant::now();
        loop {
            if let Some(guard) = self.try_acquire_path(path, operation)? {
                return Ok(guard);
            }
            if started.elapsed() >= timeout {
                bail!("the previous build is still active after {timeout:?}")
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
}

#[derive(Debug)]
pub struct GateGuard {
    file: File,
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        flock_file(&self.file, libc::LOCK_UN);
    }
}

fn runtime_directory() -> Result<PathBuf> {
    let path = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", effective_uid())));
    validate_runtime_directory(&path)?;
    Ok(path)
}

fn validate_runtime_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("runtime directory is missing: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("unsafe runtime directory: {}", path.display())
    }
    #[cfg(target_os = "linux")]
    {
        let expected_uid = effective_uid();
        if metadata.uid() != expected_uid {
            bail!(
                "runtime directory is not owned by the current user: {}",
                path.display()
            )
        }
        if metadata.permissions().mode() & 0o022 != 0 {
            bail!(
                "runtime directory is writable by other users: {}",
                path.display()
            )
        }
    }
    Ok(())
}

fn open_lock_file(path: &Path) -> Result<File> {
    let parent = path.parent().context("lock has no parent directory")?;
    validate_runtime_directory(parent)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(target_os = "linux")]
    options
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("lock is not a regular file: {}", path.display())
    }
    #[cfg(target_os = "linux")]
    {
        if metadata.uid() != effective_uid()
            || metadata.nlink() != 1
            || metadata.permissions().mode() & 0o077 != 0
        {
            bail!(
                "unsafe owner, mode, or link count on lock: {}",
                path.display()
            )
        }
    }
    Ok(file)
}

fn verify_named_inode(file: &File, path: &Path) -> Result<()> {
    let opened = file.metadata()?;
    let named = std::fs::symlink_metadata(path)?;
    #[cfg(target_os = "linux")]
    if opened.dev() != named.dev() || opened.ino() != named.ino() || named.nlink() != 1 {
        bail!("lock was replaced during acquisition: {}", path.display())
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn gate() -> (tempfile::TempDir, BuildGate) {
        let directory = tempdir().unwrap();
        #[cfg(target_os = "linux")]
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let gate = BuildGate::at(directory.path().join("build.lock"));
        (directory, gate)
    }

    #[test]
    fn one_heavy_build_excludes_only_another_heavy_build() {
        let (_directory, gate) = gate();
        let heavy = gate.acquire_heavy(Duration::from_millis(50)).unwrap();
        assert_eq!(gate.state().unwrap(), GateState::HeavyBuildActive);
        assert!(gate.acquire_heavy(Duration::from_millis(20)).is_err());
        drop(heavy);
        assert_eq!(gate.state().unwrap(), GateState::Idle);
        assert!(gate.acquire_heavy(Duration::from_millis(20)).is_ok());
    }

    #[test]
    fn lock_file_must_not_be_a_symlink() {
        let directory = tempdir().unwrap();
        #[cfg(target_os = "linux")]
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, b"safe").unwrap();
        let lock = directory.path().join("build.lock");
        std::os::unix::fs::symlink(&target, &lock).unwrap();
        let gate = BuildGate::at(lock);
        assert!(gate.acquire_heavy(Duration::from_millis(20)).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"safe");
    }
}
