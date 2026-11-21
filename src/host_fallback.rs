//! Same-boot fallback for an already identified WSL backing volume.
//!
//! A successful Windows probe records the VHDX and its DrvFs mount. Later
//! probes read live capacity through that mount, after checking the boot,
//! distribution, root device, mount source, and VHDX file identity again.

use crate::fsutil::{atomic_write_private, default_state_dir, read_private};
use crate::host::{HostPlatform, HostSnapshot, current_distro_name};
use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifiedWslIdentity {
    schema_version: u32,
    boot_id: String,
    distro: String,
    root_device: u64,
    windows_vhdx_path: String,
    windows_volume_root: String,
    sparse: bool,
    mountpoint: PathBuf,
    file_device: u64,
    file_inode: u64,
}

fn identity_path() -> Result<PathBuf> {
    Ok(default_state_dir()?.join("wsl-host-identity.json"))
}

fn boot_id() -> Result<String> {
    let value = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let value = value.trim();
    if value.len() != 36 || !value.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        bail!("invalid Linux boot identity")
    }
    Ok(value.to_owned())
}

/// Save only information obtained from a live Windows snapshot.
pub fn remember_wsl_host(snapshot: &HostSnapshot) -> Result<()> {
    snapshot.validate()?;
    snapshot.require_fresh(std::time::Duration::from_secs(60))?;
    if snapshot.platform != HostPlatform::Wsl2 {
        bail!("host identity is not WSL2")
    }
    let distro = current_distro_name()?;
    if snapshot.distro.as_deref() != Some(distro.as_str()) {
        bail!("Windows snapshot names a different distribution")
    }
    let windows_vhdx_path = snapshot.vhdx_path.clone().context("missing VHDX path")?;
    let windows_volume_root = snapshot.volume_root.clone();
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    let mountpoint = find_drvfs_mount(&mountinfo, &windows_volume_root)?;
    let file = verified_vhdx(&mountpoint, &windows_volume_root, &windows_vhdx_path)?;
    let metadata = fs::metadata(file)?;
    let identity = VerifiedWslIdentity {
        schema_version: 1,
        boot_id: boot_id()?,
        distro,
        root_device: fs::metadata("/")?.dev(),
        windows_vhdx_path,
        windows_volume_root,
        sparse: snapshot.vhdx_sparse.context("missing sparse flag")?,
        mountpoint,
        file_device: metadata.dev(),
        file_inode: metadata.ino(),
    };
    atomic_write_private(&identity_path()?, &serde_json::to_vec_pretty(&identity)?)
}

/// Return fresh physical-volume capacity, never a cached free-space number.
pub fn probe_verified_drvfs() -> Result<HostSnapshot> {
    let bytes = read_private(&identity_path()?, 64 * 1024)?;
    let identity: VerifiedWslIdentity = serde_json::from_slice(&bytes)?;
    probe_with_context(
        &identity,
        &boot_id()?,
        &current_distro_name()?,
        fs::metadata("/")?.dev(),
        &fs::read_to_string("/proc/self/mountinfo")?,
    )
}

fn probe_with_context(
    identity: &VerifiedWslIdentity,
    boot: &str,
    distro: &str,
    root_device: u64,
    mountinfo: &str,
) -> Result<HostSnapshot> {
    if identity.schema_version != 1
        || identity.boot_id != boot
        || identity.distro != distro
        || identity.root_device != root_device
    {
        bail!("cached WSL identity does not match this boot and distribution")
    }
    let mountpoint = find_drvfs_mount(mountinfo, &identity.windows_volume_root)?;
    if mountpoint != identity.mountpoint {
        bail!("the backing volume mount has changed")
    }
    let file = verified_vhdx(
        &mountpoint,
        &identity.windows_volume_root,
        &identity.windows_vhdx_path,
    )?;
    let metadata = fs::metadata(&file)?;
    if metadata.dev() != identity.file_device || metadata.ino() != identity.file_inode {
        bail!("the backing VHDX identity has changed")
    }
    let path = CString::new(mountpoint.as_os_str().as_bytes())?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: the mount path is NUL-terminated and stats is writable.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot read host volume capacity");
    }
    // SAFETY: statvfs returned success and initialized stats.
    let stats = unsafe { stats.assume_init() };
    let snapshot = HostSnapshot {
        schema_version: 2,
        platform: HostPlatform::Wsl2,
        captured_at: Timestamp::now(),
        source: Some("verified_drvfs".to_owned()),
        distro: Some(identity.distro.clone()),
        vhdx_path: Some(identity.windows_vhdx_path.clone()),
        vhdx_sparse: Some(identity.sparse),
        vhdx_size_bytes: Some(metadata.blocks().saturating_mul(512)),
        volume_root: identity.windows_volume_root.clone(),
        volume_total_bytes: stats
            .f_blocks
            .checked_mul(stats.f_frsize)
            .context("volume size overflow")?,
        volume_free_bytes: stats
            .f_bavail
            .checked_mul(stats.f_frsize)
            .context("volume free-space overflow")?,
    };
    snapshot.validate()?;
    Ok(snapshot)
}

fn drive_root(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    if bytes.len() != 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'\\' | b'/')
    {
        bail!("backing volume is not a drive-letter root")
    }
    Ok(format!("{}:\\", char::from(bytes[0].to_ascii_uppercase())))
}

fn verified_vhdx(mountpoint: &Path, volume: &str, windows_path: &str) -> Result<PathBuf> {
    let volume = drive_root(volume)?;
    let normalized = windows_path.replace('/', "\\");
    let prefix = normalized.get(..3).context("invalid VHDX path prefix")?;
    if normalized.len() < 4 || !prefix.eq_ignore_ascii_case(&volume) {
        bail!("VHDX path is outside the backing volume")
    }
    let mut relative = PathBuf::new();
    for part in normalized[3..].split('\\') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.contains(':')
            || part.contains('\0')
        {
            bail!("invalid VHDX path component")
        }
        relative.push(part);
    }
    if !relative
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("ext4.vhdx"))
    {
        bail!("backing file is not ext4.vhdx")
    }
    let mountpoint = fs::canonicalize(mountpoint)?;
    let file = fs::canonicalize(mountpoint.join(relative))?;
    let metadata = fs::metadata(&file)?;
    if !file.starts_with(&mountpoint)
        || !metadata.is_file()
        || metadata.dev() != fs::metadata(&mountpoint)?.dev()
    {
        bail!("VHDX escaped the verified backing mount")
    }
    Ok(file)
}

fn find_drvfs_mount(mountinfo: &str, volume: &str) -> Result<PathBuf> {
    let volume = drive_root(volume)?;
    let mut matches = Vec::new();
    for line in mountinfo.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let Some(mountpoint) = left.split_whitespace().nth(4) else {
            continue;
        };
        let mut right = right.split_whitespace();
        let Some(kind) = right.next() else { continue };
        let Some(source) = right.next() else { continue };
        if !matches!(kind, "9p" | "drvfs") {
            continue;
        }
        let source = unescape_mount_field(source)?;
        if source.eq_ignore_ascii_case(&volume) {
            matches.push(PathBuf::from(unescape_mount_field(mountpoint)?));
        }
    }
    if matches.len() != 1 || !matches[0].is_absolute() {
        bail!("backing volume has no unique DrvFs mount")
    }
    Ok(matches.remove(0))
}

fn unescape_mount_field(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && bytes[index + 1..index + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let octal = &bytes[index + 1..index + 4];
            let decoded = (octal[0] - b'0') as u16 * 64
                + (octal[1] - b'0') as u16 * 8
                + (octal[2] - b'0') as u16;
            output.push(u8::try_from(decoded).context("invalid mount escape")?);
            index += 4;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    Ok(String::from_utf8(output)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_requires_matching_boot_mount_and_file_identity() {
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("drive");
        fs::create_dir(&mountpoint).unwrap();
        let vhdx = mountpoint.join("ext4.vhdx");
        fs::write(&vhdx, b"fixture").unwrap();
        let metadata = fs::metadata(&vhdx).unwrap();
        let mountinfo = format!("1 0 0:1 / {} rw - 9p I:\\134 rw\n", mountpoint.display());
        let mut identity = VerifiedWslIdentity {
            schema_version: 1,
            boot_id: "boot-a".into(),
            distro: "Fixture".into(),
            root_device: 11,
            windows_vhdx_path: "I:\\ext4.vhdx".into(),
            windows_volume_root: "I:\\".into(),
            sparse: true,
            mountpoint: mountpoint.clone(),
            file_device: metadata.dev(),
            file_inode: metadata.ino(),
        };
        let snapshot = probe_with_context(&identity, "boot-a", "Fixture", 11, &mountinfo).unwrap();
        assert_eq!(snapshot.source.as_deref(), Some("verified_drvfs"));
        assert_eq!(snapshot.vhdx_path.as_deref(), Some("I:\\ext4.vhdx"));
        assert!(snapshot.volume_free_bytes <= snapshot.volume_total_bytes);
        assert!(probe_with_context(&identity, "boot-b", "Fixture", 11, &mountinfo).is_err());
        assert!(probe_with_context(&identity, "boot-a", "Other", 11, &mountinfo).is_err());
        assert!(probe_with_context(&identity, "boot-a", "Fixture", 12, &mountinfo).is_err());
        assert!(
            probe_with_context(
                &identity,
                "boot-a",
                "Fixture",
                11,
                &mountinfo.replace("I:", "J:")
            )
            .is_err()
        );
        identity.file_inode ^= 1;
        assert!(probe_with_context(&identity, "boot-a", "Fixture", 11, &mountinfo).is_err());
    }

    #[test]
    fn windows_paths_cannot_escape_the_verified_mount() {
        let dir = tempfile::tempdir().unwrap();
        assert!(verified_vhdx(dir.path(), "I:\\", "I:\\..\\ext4.vhdx").is_err());
        assert!(verified_vhdx(dir.path(), "I:\\", "J:\\ext4.vhdx").is_err());
        assert!(find_drvfs_mount("1 0 0:1 / /mnt/i rw - ext4 I:\\134 rw", "I:\\").is_err());
        assert_eq!(
            unescape_mount_field("/mnt/My\\040Drive").unwrap(),
            "/mnt/My Drive"
        );
    }
}
