//! A single read-only host probe. It never starts, pauses, or stops a VM or WSL.

use crate::config::DiskConfig;
use crate::fsutil::{default_state_dir, read_private};
use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::ffi::{CString, OsStr};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
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
# FileInfo.Length includes holes in a sparse VHDX. The volume probe remains
# usable when Windows policy prevents querying the allocation of this file.
$vhdxSize = $null
try {
  Add-Type -Namespace GuardWSL -Name FileAllocation -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("kernel32.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode, ExactSpelling = true, SetLastError = true)]
private static extern uint GetCompressedFileSizeW(string path, out uint high);
[System.Runtime.InteropServices.DllImport("kernel32.dll", ExactSpelling = true)]
private static extern void SetLastError(uint error);
public static ulong Read(string path) {
    uint high;
    SetLastError(0);
    uint low = GetCompressedFileSizeW(path, out high);
    int error = System.Runtime.InteropServices.Marshal.GetLastWin32Error();
    if (low == uint.MaxValue && error != 0) throw new System.ComponentModel.Win32Exception(error);
    return ((ulong)high << 32) | low;
}
'@
  $vhdxSize = [GuardWSL.FileAllocation]::Read($vhdxItem.FullName)
} catch { $vhdxSize = $null }
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
  vhdx_size_bytes = $vhdxSize
  volume_root = $volumeRoot
  volume_total_bytes = $volumeSize
  volume_free_bytes = $volumeFree
} | ConvertTo-Json -Compress
"#;
const MAX_PROBE_OUTPUT_BYTES: usize = 1024 * 1024;
static PENDING_HOST_PROBES: AtomicUsize = AtomicUsize::new(0);
static PENDING_CLEANUP_COMMANDS: AtomicUsize = AtomicUsize::new(0);
static PENDING_TRIMS: AtomicUsize = AtomicUsize::new(0);

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
    pub captured_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distro: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vhdx_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vhdx_sparse: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vhdx_size_bytes: Option<u64>,
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
                self.distro.is_none()
                    && self.vhdx_path.is_none()
                    && self.vhdx_sparse.is_none()
                    && self.vhdx_size_bytes.is_none()
            }
        };
        if !wsl_identity_valid {
            bail!("host snapshot has invalid platform identity")
        }
        if self.volume_root.chars().any(char::is_control)
            || self
                .source
                .as_ref()
                .is_some_and(|value| value.chars().any(char::is_control))
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
        let age = Timestamp::now().duration_since(self.captured_at);
        const MAX_CLOCK_SKEW_MILLISECONDS: i128 = 5_000;
        let age_milliseconds = age.as_millis();
        let max_age_milliseconds = i128::try_from(max_age.as_millis()).unwrap_or(i128::MAX);
        if !(-MAX_CLOCK_SKEW_MILLISECONDS..=max_age_milliseconds).contains(&age_milliseconds) {
            bail!("host snapshot is stale: {age_milliseconds} ms")
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

/// Fraction of the Windows volume occupied by the backing VHDX at which
/// offline compaction becomes the dominant risk, independent of free bytes.
pub const VHDX_OCCUPANCY_ADVISORY: f64 = 0.75;

/// Whether deleting Linux files can return physical bytes to the host volume.
///
/// Native Linux deletion reclaims the local filesystem directly. On WSL2 a
/// sparse VHDX may return host blocks gradually; a non-sparse VHDX never
/// shrinks on delete, so logical cleanup cannot reclaim host space at all.
#[must_use]
pub fn host_reclaim_possible(snapshot: &HostSnapshot) -> bool {
    match snapshot.platform {
        HostPlatform::Linux => true,
        HostPlatform::Wsl2 => snapshot.vhdx_sparse != Some(false),
    }
}

/// VHDX physical size as a fraction of the backing Windows volume.
#[must_use]
pub fn vhdx_occupancy_ratio(snapshot: &HostSnapshot) -> Option<f64> {
    let size = snapshot.vhdx_size_bytes?;
    if snapshot.volume_total_bytes == 0 {
        return None;
    }
    Some(size as f64 / snapshot.volume_total_bytes as f64)
}

/// True when host space cannot be recovered by cleanup alone and the user must
/// compact the VHDX offline. GuardWSL never compacts, converts, or shuts down
/// WSL; this is an advisory so pressure is not mistaken for a cleanup failure.
#[must_use]
pub fn compaction_advised(
    snapshot: Option<&HostSnapshot>,
    pressure: Option<DiskPressure>,
    deleted_logical_bytes: u64,
    host_free_delta_bytes: Option<i64>,
) -> bool {
    let Some(snapshot) = snapshot else {
        return false;
    };
    if host_reclaim_possible(snapshot) {
        return false;
    }
    let high_occupancy =
        vhdx_occupancy_ratio(snapshot).is_some_and(|ratio| ratio >= VHDX_OCCUPANCY_ADVISORY);
    let under_pressure = pressure.is_some_and(|current| current != DiskPressure::Healthy);
    let useless_reclaim =
        deleted_logical_bytes > 0 && host_free_delta_bytes.is_none_or(|delta| delta <= 0);
    under_pressure || high_occupancy || useless_reclaim
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
        captured_at: Timestamp::now(),
        source: None,
        distro: None,
        vhdx_path: None,
        vhdx_sparse: None,
        vhdx_size_bytes: None,
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
        if output.stdout_truncated || output.stderr_truncated {
            bail!("PowerShell probe output exceeded the capture limit")
        }
        parse_snapshot(&output.stdout)
    }
}

pub(crate) fn current_distro_name() -> Result<String> {
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
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
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
    run_bounded_inner(
        program,
        args,
        timeout,
        environment,
        MAX_PROBE_OUTPUT_BYTES,
        &PENDING_HOST_PROBES,
    )
}

pub(crate) fn run_bounded_cleanup_command<I, S>(
    program: &str,
    args: I,
    timeout: Duration,
    output_limit: usize,
) -> Result<BoundedOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_bounded_inner(
        program,
        args,
        timeout,
        &[],
        output_limit,
        &PENDING_CLEANUP_COMMANDS,
    )
}

pub fn run_bounded_trim<I, S>(program: &str, args: I, timeout: Duration) -> Result<BoundedOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_bounded_inner(program, args, timeout, &[], 64 * 1024, &PENDING_TRIMS)
}

fn run_bounded_inner<I, S>(
    program: &str,
    args: I,
    timeout: Duration,
    environment: &[(&str, &str)],
    output_limit: usize,
    pending: &'static AtomicUsize,
) -> Result<BoundedOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    if pending.load(Ordering::Acquire) != 0 {
        bail!("a previous timed-out {program} process is still being reaped")
    }
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
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        abandon_child(child, pending);
        bail!("{program} did not provide output pipes")
    };
    if let Err(error) = set_nonblocking(&stdout).and_then(|()| set_nonblocking(&stderr)) {
        abandon_child(child, pending);
        return Err(error).context("could not configure nonblocking output pipes");
    }
    let mut stdout_data = Vec::new();
    let mut stderr_data = Vec::new();
    let mut stdout_truncated = false;
    let mut stderr_truncated = false;
    let mut stdout_closed = false;
    let mut stderr_closed = false;
    let mut status = None;
    let started = Instant::now();
    loop {
        if !stdout_closed {
            match drain_nonblocking(
                &mut stdout,
                &mut stdout_data,
                output_limit,
                &mut stdout_truncated,
            ) {
                Ok(closed) => stdout_closed = closed,
                Err(error) => {
                    if status.is_none() {
                        abandon_child(child, pending);
                    }
                    return Err(error).context("could not read process stdout");
                }
            }
        }
        if !stderr_closed {
            match drain_nonblocking(
                &mut stderr,
                &mut stderr_data,
                output_limit,
                &mut stderr_truncated,
            ) {
                Ok(closed) => stderr_closed = closed,
                Err(error) => {
                    if status.is_none() {
                        abandon_child(child, pending);
                    }
                    return Err(error).context("could not read process stderr");
                }
            }
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(result) => status = result,
                Err(error) => {
                    abandon_child(child, pending);
                    return Err(error).context("could not check process status");
                }
            }
        }
        if let Some(status) = status
            && stdout_closed
            && stderr_closed
        {
            return Ok(BoundedOutput {
                status,
                stdout: stdout_data,
                stderr: stderr_data,
                stdout_truncated,
                stderr_truncated,
            });
        }
        if started.elapsed() >= timeout {
            if status.is_none() {
                abandon_child(child, pending);
            }
            bail!("{program} exceeded the {timeout:?} timeout")
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn set_nonblocking(stream: &impl AsRawFd) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    // SAFETY: fcntl only reads and changes flags on the live pipe descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is valid and the flags retain every preexisting bit.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain_nonblocking<R: Read>(
    reader: &mut R,
    output: &mut Vec<u8>,
    limit: usize,
    truncated: &mut bool,
) -> io::Result<bool> {
    let mut buffer = [0_u8; 8192];
    for _ in 0..16 {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(read) => {
                let remaining = limit.saturating_sub(output.len());
                output.extend_from_slice(&buffer[..read.min(remaining)]);
                *truncated |= read > remaining;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn abandon_child(mut child: Child, pending: &'static AtomicUsize) {
    let _ = child.kill();
    pending.fetch_add(1, Ordering::AcqRel);
    // A child stuck in uninterruptible sleep must not stall cleanup. If a
    // reaper cannot be created, the nonzero count deliberately prevents more
    // subprocesses from accumulating in this process.
    let _ = thread::Builder::new()
        .name("guard-process-reaper".to_owned())
        .spawn(move || {
            let _ = child.wait();
            pending.fetch_sub(1, Ordering::AcqRel);
        });
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
          "vhdx_size_bytes":123456789,
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
    fn missing_file_allocation_preserves_host_capacity_and_requires_vhdx_identity() {
        let raw = r#"{
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
        let snapshot = parse_snapshot(raw.as_bytes()).unwrap();
        assert_eq!(snapshot.volume_free_bytes, 500);
        assert_eq!(vhdx_occupancy_ratio(&snapshot), None);
        let missing_identity = raw.replace("\"vhdx_sparse\":false,", "");
        assert!(parse_snapshot(missing_identity.as_bytes()).is_err());
    }

    #[test]
    fn freshness_tolerates_small_windows_wsl_clock_skew() {
        let mut snapshot = HostSnapshot {
            schema_version: 2,
            platform: HostPlatform::Wsl2,
            captured_at: Timestamp::now() + jiff::SignedDuration::from_secs(2),
            source: None,
            distro: Some("Example-WSL".to_owned()),
            vhdx_path: Some(r"X:\WSL\Example-WSL\ext4.vhdx".to_owned()),
            vhdx_sparse: Some(false),
            vhdx_size_bytes: Some(120 * GIB),
            volume_root: r"X:\".to_owned(),
            volume_total_bytes: 200 * GIB,
            volume_free_bytes: 80 * GIB,
        };
        snapshot.require_fresh(Duration::from_secs(10)).unwrap();
        snapshot.captured_at = Timestamp::now() + jiff::SignedDuration::from_secs(10);
        assert!(snapshot.require_fresh(Duration::from_secs(10)).is_err());
    }

    fn wsl_snapshot(sparse: bool, vhdx_size_bytes: u64, total: u64, free: u64) -> HostSnapshot {
        HostSnapshot {
            schema_version: 2,
            platform: HostPlatform::Wsl2,
            captured_at: Timestamp::now(),
            source: None,
            distro: Some("Example-WSL".to_owned()),
            vhdx_path: Some(r"X:\WSL\Example-WSL\ext4.vhdx".to_owned()),
            vhdx_sparse: Some(sparse),
            vhdx_size_bytes: Some(vhdx_size_bytes),
            volume_root: r"X:\".to_owned(),
            volume_total_bytes: total,
            volume_free_bytes: free,
        }
    }

    #[test]
    fn non_sparse_vhdx_reports_no_host_reclaim() {
        let dense = wsl_snapshot(false, 180 * GIB, 200 * GIB, 10 * GIB);
        assert!(!host_reclaim_possible(&dense));
        let sparse = wsl_snapshot(true, 180 * GIB, 200 * GIB, 10 * GIB);
        assert!(host_reclaim_possible(&sparse));
        let linux = HostSnapshot {
            schema_version: 2,
            platform: HostPlatform::Linux,
            captured_at: Timestamp::now(),
            source: None,
            distro: None,
            vhdx_path: None,
            vhdx_sparse: None,
            vhdx_size_bytes: None,
            volume_root: "/home".to_owned(),
            volume_total_bytes: 200 * GIB,
            volume_free_bytes: 80 * GIB,
        };
        assert!(host_reclaim_possible(&linux));
    }

    #[test]
    fn vhdx_occupancy_uses_physical_file_over_volume() {
        let snapshot = wsl_snapshot(false, 150 * GIB, 200 * GIB, 50 * GIB);
        let ratio = vhdx_occupancy_ratio(&snapshot).unwrap();
        assert!((ratio - 0.75).abs() < 1e-9);
    }

    #[test]
    fn compaction_is_advised_only_when_host_space_cannot_return() {
        let dense = wsl_snapshot(false, 180 * GIB, 200 * GIB, 10 * GIB);
        // Under pressure on a non-sparse VHDX.
        assert!(compaction_advised(
            Some(&dense),
            Some(DiskPressure::Pressure),
            0,
            Some(0)
        ));
        // Logical cleanup removed bytes and the host did not move.
        assert!(compaction_advised(
            Some(&dense),
            Some(DiskPressure::Healthy),
            100,
            Some(0)
        ));
        // High occupancy alone is enough.
        assert!(compaction_advised(
            Some(&dense),
            Some(DiskPressure::Healthy),
            0,
            Some(5 * GIB as i64)
        ));
        // Healthy, low occupancy, no futile cleanup.
        let roomy = wsl_snapshot(false, 40 * GIB, 200 * GIB, 150 * GIB);
        assert!(!compaction_advised(
            Some(&roomy),
            Some(DiskPressure::Healthy),
            0,
            Some(1)
        ));
        // Sparse VHDX can reclaim; never advise compaction.
        let sparse = wsl_snapshot(true, 180 * GIB, 200 * GIB, 10 * GIB);
        assert!(!compaction_advised(
            Some(&sparse),
            Some(DiskPressure::Emergency),
            0,
            Some(0)
        ));
        // No snapshot, no advice.
        assert!(!compaction_advised(
            None,
            Some(DiskPressure::Emergency),
            0,
            Some(0)
        ));
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
        assert!(output.stdout_truncated);
        assert!(output.stderr_truncated);
    }

    #[test]
    fn bounded_runner_does_not_wait_for_descendant_holding_pipes() {
        let started = Instant::now();
        let result = run_bounded(
            "/bin/sh",
            ["-c", "sleep 2 &"],
            Duration::from_millis(100),
            &[],
        );
        assert!(result.is_err(), "an open descendant pipe must time out");
        assert!(started.elapsed() < Duration::from_secs(1));
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
