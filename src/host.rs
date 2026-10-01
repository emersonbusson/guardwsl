//! A single read-only host probe. It never starts, pauses, or stops a VM or WSL.

use crate::config::DiskConfig;
use crate::fsutil::{default_state_dir, read_private};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::ffi::{CString, OsStr};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const HOST_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$distroName = $env:GUARDWSL_DISTRO
if ([string]::IsNullOrWhiteSpace($distroName)) { throw 'GUARDWSL_DISTRO is missing' }
$matches = @(
  Get-ChildItem -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Lxss' |
    ForEach-Object {
      $item = Get-ItemProperty -LiteralPath $_.PSPath
      if ($item.DistributionName -eq $distroName) { $item }
    }
)
if ($matches.Count -ne 1) { throw "distribution appears $($matches.Count) time(s) in the registry" }
$basePath = [Environment]::ExpandEnvironmentVariables([string]$matches[0].BasePath)
$vhdxPath = Join-Path -Path $basePath -ChildPath 'ext4.vhdx'
if (-not (Test-Path -LiteralPath $vhdxPath -PathType Leaf)) { throw "ext4.vhdx is missing: $vhdxPath" }
$vhdxItem = Get-Item -LiteralPath $vhdxPath
$vhdxSparse = [bool]($vhdxItem.Attributes -band [System.IO.FileAttributes]::SparseFile)
try {
  $volume = Get-Volume -FilePath $vhdxPath -ErrorAction Stop
  $volumeRoot = if ($volume.DriveLetter) { "$($volume.DriveLetter):\" } else { [string]$volume.Path }
  $volumeSize = [uint64]$volume.Size
  $volumeFree = [uint64]$volume.SizeRemaining
} catch {
  $root = [System.IO.Path]::GetPathRoot($vhdxPath)
  $drive = [System.IO.DriveInfo]::new($root)
  $volumeRoot = [string]$drive.RootDirectory.FullName
  $volumeSize = [uint64]$drive.TotalSize
  $volumeFree = [uint64]$drive.AvailableFreeSpace
}
[pscustomobject]@{
  schema_version = 2
  platform = 'wsl2'
  captured_at = (Get-Date).ToUniversalTime().ToString('o')
  distro = $distroName
  vhdx_path = $vhdxPath
  vhdx_sparse = $vhdxSparse
  volume_root = $volumeRoot
  volume_total_bytes = $volumeSize
  volume_free_bytes = $volumeFree
} | ConvertTo-Json -Compress
"#;
const MAX_PROBE_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostPlatform {
    Linux,
    Wsl2,
}

pub fn classify_platform(kernel_release: &str) -> Result<HostPlatform> {
    let release = kernel_release.to_ascii_lowercase();
    if release.contains("microsoft") {
        if release.contains("wsl2") {
            Ok(HostPlatform::Wsl2)
        } else {
            bail!("WSL1 is not supported")
        }
    } else {
        Ok(HostPlatform::Linux)
    }
}

pub fn current_platform() -> Result<HostPlatform> {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .context("could not identify the Linux kernel")?;
    classify_platform(release.trim())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSnapshot {
    pub schema_version: u32,
    pub platform: HostPlatform,
    pub captured_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distro: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vhdx_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vhdx_sparse: Option<bool>,
    pub volume_root: String,
    pub volume_total_bytes: u64,
    pub volume_free_bytes: u64,
}

impl HostSnapshot {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 2
            || self.volume_root.trim().is_empty()
            || self.volume_total_bytes == 0
            || self.volume_free_bytes > self.volume_total_bytes
        {
            bail!("host snapshot has invalid geometry or identity")
        }
        let wsl_identity_valid = match self.platform {
            HostPlatform::Wsl2 => self
                .distro
                .as_ref()
                .zip(self.vhdx_path.as_ref())
                .is_some_and(|(distro, path)| {
                    !distro.trim().is_empty()
                        && !path.trim().is_empty()
                        && self.vhdx_sparse.is_some()
                }),
            HostPlatform::Linux => {
                self.distro.is_none() && self.vhdx_path.is_none() && self.vhdx_sparse.is_none()
            }
        };
        if !wsl_identity_valid {
            bail!("host snapshot has invalid platform identity")
        }
        if self.volume_root.chars().any(char::is_control)
            || self
                .distro
                .as_ref()
                .is_some_and(|value| value.chars().any(char::is_control))
            || self
                .vhdx_path
                .as_ref()
                .is_some_and(|value| value.chars().any(char::is_control))
        {
            bail!("host snapshot contains a control character")
        }
        Ok(())
    }

    pub fn require_fresh(&self, max_age: Duration) -> Result<()> {
        let age = Utc::now().signed_duration_since(self.captured_at);
        const MAX_CLOCK_SKEW_MILLISECONDS: i64 = 5_000;
        let age_milliseconds = age.num_milliseconds();
        if age_milliseconds < -MAX_CLOCK_SKEW_MILLISECONDS
            || (age_milliseconds >= 0
                && u64::try_from(age_milliseconds).unwrap_or(u64::MAX)
                    > u64::try_from(max_age.as_millis()).unwrap_or(u64::MAX))
        {
            bail!("host snapshot is stale: {} ms", age.num_milliseconds())
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskPressure {
    Healthy,
    Pressure,
    Critical,
    Emergency,
}

impl DiskPressure {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Pressure => "pressure",
            Self::Critical => "critical",
            Self::Emergency => "emergency",
        }
    }
}

#[must_use]
pub fn classify_disk(free_bytes: u64, total_bytes: u64, config: &DiskConfig) -> DiskPressure {
    let thresholds = config.effective_thresholds(total_bytes);
    if free_bytes <= thresholds.emergency_free_bytes {
        DiskPressure::Emergency
    } else if free_bytes <= thresholds.critical_free_bytes {
        DiskPressure::Critical
    } else if free_bytes <= thresholds.pressure_free_bytes {
        DiskPressure::Pressure
    } else {
        DiskPressure::Healthy
    }
}

pub fn probe_linux_disk(scan_roots: &[PathBuf]) -> Result<HostSnapshot> {
    let (first, rest) = scan_roots
        .split_first()
        .context("no cleanup scan root configured")?;
    let first_meta = std::fs::symlink_metadata(first)
        .with_context(|| format!("could not inspect scan root {}", first.display()))?;
    if !first.is_absolute() || first_meta.file_type().is_symlink() || !first_meta.is_dir() {
        bail!("unsafe Linux scan root: {}", first.display())
    }
    let device = first_meta.dev();
    for root in rest {
        let metadata = std::fs::symlink_metadata(root)
            .with_context(|| format!("could not inspect scan root {}", root.display()))?;
        if !root.is_absolute()
            || metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.dev() != device
        {
            bail!("Linux scan roots must be directories on one filesystem")
        }
    }
    let canonical = std::fs::canonicalize(first)?;
    let path = CString::new(canonical.as_os_str().as_bytes())?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: the path is NUL-terminated and stats points to writable storage.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error()).context("could not read Linux filesystem capacity");
    }
    // SAFETY: statvfs returned success and initialized the output structure.
    let stats = unsafe { stats.assume_init() };
    let block_size = stats.f_frsize;
    let total = stats
        .f_blocks
        .checked_mul(block_size)
        .context("filesystem size overflow")?;
    let free = stats
        .f_bavail
        .checked_mul(block_size)
        .context("filesystem free-space overflow")?;
    let snapshot = HostSnapshot {
        schema_version: 2,
        platform: HostPlatform::Linux,
        captured_at: Utc::now(),
        distro: None,
        vhdx_path: None,
        vhdx_sparse: None,
        volume_root: canonical
            .to_str()
            .context("scan root is not UTF-8")?
            .to_owned(),
        volume_total_bytes: total,
        volume_free_bytes: free,
    };
    snapshot.validate()?;
    Ok(snapshot)
}

#[derive(Debug, Clone)]
pub struct PowerShellHostProbe {
    timeout: Duration,
}

impl PowerShellHostProbe {
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    pub fn probe(&self) -> Result<HostSnapshot> {
        let distro = current_distro_name()?;
        let interop = discover_wsl_interop()?;
        let program = powershell_program();
        let program = program.to_str().context("PowerShell path is not UTF-8")?;
        let wslenv = forwarded_wslenv(
            &std::env::var("WSLENV").unwrap_or_default(),
            "GUARDWSL_DISTRO",
        );
        let output = run_bounded(
            program,
            [
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                HOST_SCRIPT,
            ],
            self.timeout,
            &[
                ("GUARDWSL_DISTRO", distro.as_str()),
                ("WSLENV", wslenv.as_str()),
                ("WSL_INTEROP", interop.as_str()),
            ],
        )?;
        if !output.status.success() {
            bail!(
                "PowerShell probe failed ({}): {}",
                output.status,
                normalize_windows_text(&output.stderr)
            )
        }
        parse_snapshot(&output.stdout)
    }
}

fn current_distro_name() -> Result<String> {
    if let Ok(distro) = std::env::var("WSL_DISTRO_NAME") {
        return validate_distro_name(distro);
    }
    let path = default_state_dir()?.join("distro-name");
    let bytes = read_private(&path, 256)?;
    let distro = std::str::from_utf8(&bytes)
        .context("distro-name is not UTF-8")?
        .trim()
        .to_owned();
    validate_distro_name(distro)
}

fn validate_distro_name(distro: String) -> Result<String> {
    // Matches the installer's `^[[:alnum:]._-]+$` so a name accepted at install
    // time is also accepted at probe time, and anything else is rejected before
    // it can reach the Windows host probe.
    if distro.is_empty()
        || distro.len() > 128
        || !distro
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        bail!("invalid WSL distribution name")
    }
    Ok(distro)
}

fn powershell_program() -> PathBuf {
    let system = PathBuf::from("/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe");
    if system.is_file() {
        system
    } else {
        PathBuf::from("powershell.exe")
    }
}

fn discover_wsl_interop() -> Result<String> {
    let configured = std::env::var_os("WSL_INTEROP").map(PathBuf::from);
    let path = discover_wsl_interop_at(Path::new("/run/WSL"), configured.as_deref())?;
    path.into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("WSL_INTEROP path is not UTF-8"))
}

fn discover_wsl_interop_at(root: &Path, configured: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = configured
        && is_interop_socket(path)
    {
        return Ok(path.to_path_buf());
    }
    let mut candidates = std::fs::read_dir(root)
        .with_context(|| format!("could not read {}", root.display()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with("_interop"))
        .filter(|entry| is_interop_socket(&entry.path()))
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.file_name(), entry.path()))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
    candidates
        .pop()
        .map(|(_, _, path)| path)
        .context("no active WSL_INTEROP socket found")
}

fn is_interop_socket(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        !metadata.file_type().is_symlink() && metadata.file_type().is_socket()
    })
}

fn forwarded_wslenv(existing: &str, variable: &str) -> String {
    if existing
        .split(':')
        .filter(|entry| !entry.is_empty())
        .any(|entry| entry.split('/').next() == Some(variable))
    {
        return existing.to_owned();
    }
    if existing.is_empty() {
        variable.to_owned()
    } else {
        format!("{existing}:{variable}")
    }
}

pub struct BoundedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub fn run_bounded<I, S>(
    program: &str,
    args: I,
    timeout: Duration,
    environment: &[(&str, &str)],
) -> Result<BoundedOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in environment {
        command.env(name, value);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("could not start {program}"))?;
    let stdout_reader = child
        .stdout
        .take()
        .map(|stream| thread::spawn(move || read_capped(stream, MAX_PROBE_OUTPUT_BYTES)));
    let stderr_reader = child
        .stderr
        .take()
        .map(|stream| thread::spawn(move || read_capped(stream, MAX_PROBE_OUTPUT_BYTES)));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} exceeded the {timeout:?} timeout")
        }
        thread::sleep(Duration::from_millis(50));
    };
    let stdout = join_reader(stdout_reader, "stdout")?;
    let stderr = join_reader(stderr_reader, "stderr")?;
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_capped<R: Read>(mut reader: R, limit: usize) -> io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(output)
}

fn join_reader(
    reader: Option<thread::JoinHandle<io::Result<Vec<u8>>>>,
    stream: &str,
) -> Result<Vec<u8>> {
    let Some(reader) = reader else {
        return Ok(Vec::new());
    };
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("{stream} reader panicked"))?
        .with_context(|| format!("could not read {stream}"))
}

fn parse_snapshot(bytes: &[u8]) -> Result<HostSnapshot> {
    let text = normalize_windows_text(bytes);
    let start = text.find('{').context("probe JSON is missing")?;
    let end = text.rfind('}').context("probe JSON is incomplete")?;
    let snapshot: HostSnapshot = serde_json::from_str(&text[start..=end])?;
    snapshot.validate()?;
    Ok(snapshot)
}

fn normalize_windows_text(bytes: &[u8]) -> String {
    let filtered = bytes
        .iter()
        .copied()
        .filter(|byte| *byte != 0)
        .collect::<Vec<_>>();
    String::from_utf8_lossy(&filtered).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GIB, GuardConfig};

    #[test]
    fn pressure_thresholds_are_total_and_ordered() {
        let config = GuardConfig::default();
        assert_eq!(
            classify_disk(config.disk.target_free_bytes, 500 * GIB, &config.disk),
            DiskPressure::Healthy
        );
        assert_eq!(
            classify_disk(config.disk.pressure_free_bytes, 500 * GIB, &config.disk),
            DiskPressure::Pressure
        );
        assert_eq!(
            classify_disk(config.disk.critical_free_bytes, 500 * GIB, &config.disk),
            DiskPressure::Critical
        );
        assert_eq!(
            classify_disk(config.disk.emergency_free_bytes, 500 * GIB, &config.disk),
            DiskPressure::Emergency
        );
    }

    #[test]
    fn nul_padded_powershell_json_is_accepted() {
        let raw = br#"{
          "schema_version":2,
          "platform":"wsl2",
          "captured_at":"2026-08-23T12:00:00Z",
          "distro":"Example-WSL",
          "vhdx_path":"X:\\WSL\\Example-WSL\\ext4.vhdx",
          "vhdx_sparse":false,
          "volume_root":"X:\\",
          "volume_total_bytes":1000,
          "volume_free_bytes":500
        }"#;
        let padded = raw.iter().flat_map(|byte| [*byte, 0]).collect::<Vec<_>>();
        assert_eq!(
            parse_snapshot(&padded).unwrap().distro.as_deref(),
            Some("Example-WSL")
        );
    }

    #[test]
    fn freshness_tolerates_small_windows_wsl_clock_skew() {
        let mut snapshot = HostSnapshot {
            schema_version: 2,
            platform: HostPlatform::Wsl2,
            captured_at: Utc::now() + chrono::Duration::seconds(2),
            distro: Some("Example-WSL".to_owned()),
            vhdx_path: Some(r"X:\WSL\Example-WSL\ext4.vhdx".to_owned()),
            vhdx_sparse: Some(false),
            volume_root: r"X:\".to_owned(),
            volume_total_bytes: 200 * GIB,
            volume_free_bytes: 80 * GIB,
        };
        snapshot.require_fresh(Duration::from_secs(10)).unwrap();
        snapshot.captured_at = Utc::now() + chrono::Duration::seconds(10);
        assert!(snapshot.require_fresh(Duration::from_secs(10)).is_err());
    }

    #[test]
    fn bounded_runner_drains_output_larger_than_its_capture_limit() {
        let output = run_bounded(
            "sh",
            [
                "-c",
                "head -c 2097152 /dev/zero; head -c 2097152 /dev/zero >&2",
            ],
            Duration::from_secs(5),
            &[],
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), MAX_PROBE_OUTPUT_BYTES);
        assert_eq!(output.stderr.len(), MAX_PROBE_OUTPUT_BYTES);
    }

    #[test]
    fn wslenv_forwarding_preserves_existing_entries_without_duplicates() {
        assert_eq!(forwarded_wslenv("", "GUARDWSL_DISTRO"), "GUARDWSL_DISTRO");
        assert_eq!(
            forwarded_wslenv("PATH/l:HOME/p", "GUARDWSL_DISTRO"),
            "PATH/l:HOME/p:GUARDWSL_DISTRO"
        );
        assert_eq!(
            forwarded_wslenv("GUARDWSL_DISTRO/u:PATH/l", "GUARDWSL_DISTRO"),
            "GUARDWSL_DISTRO/u:PATH/l"
        );
    }

    #[test]
    fn distro_names_allow_only_installer_safe_characters() {
        assert_eq!(
            validate_distro_name("Ubuntu-22.04".to_owned()).unwrap(),
            "Ubuntu-22.04"
        );
        assert_eq!(
            validate_distro_name("Example_WSL.1".to_owned()).unwrap(),
            "Example_WSL.1"
        );
        for invalid in [
            "",
            "has space",
            "semi;colon",
            "dollar$var",
            "quote\"name",
            "back\\slash",
            "path/name",
            "newline\nname",
            &"x".repeat(129),
        ] {
            assert!(
                validate_distro_name((*invalid).to_owned()).is_err(),
                "expected {invalid:?} to be rejected"
            );
        }
    }

    #[test]
    fn interop_discovery_accepts_only_a_real_unix_socket() {
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let invalid = directory.path().join("1_interop");
        std::fs::write(&invalid, b"not a socket").unwrap();
        let socket = directory.path().join("2_interop");
        let _listener = UnixListener::bind(&socket).unwrap();
        assert_eq!(
            discover_wsl_interop_at(directory.path(), Some(&invalid)).unwrap(),
            socket
        );
    }
}
