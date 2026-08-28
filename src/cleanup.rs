//! Conservative cleanup for GuardWSL v1.
//!
//! Only exact names and contexts enter a plan. dist, build, out, databases,
//! Docker volumes, and unknown paths are never candidates.

use crate::config::GuardConfig;
use crate::fsutil::effective_uid;
use crate::history::{AuditLog, AuditOutcome, AuditRecord};
use crate::host::DiskPressure;
use crate::repository::{Repository, discover_repositories};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::fs;
use std::io;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use walkdir::{DirEntry, WalkDir};

const MAX_TREE_ENTRIES: usize = 2_000_000;
const MAX_DISCOVERED_PROJECTS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupKind {
    JavaScriptCache,
    RustCache,
    GoCache,
    ProjectCache,
    RustTarget,
    NextBuild,
    NodeModules,
}

impl CleanupKind {
    const fn risk_rank(self) -> u8 {
        match self {
            Self::JavaScriptCache | Self::RustCache | Self::GoCache | Self::ProjectCache => 0,
            Self::RustTarget | Self::NextBuild => 1,
            Self::NodeModules => 2,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::JavaScriptCache => "javascript_cache",
            Self::RustCache => "rust_cache",
            Self::GoCache => "go_cache",
            Self::ProjectCache => "project_cache",
            Self::RustTarget => "rust_target",
            Self::NextBuild => "next_build",
            Self::NodeModules => "node_modules",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupCandidate {
    pub path: PathBuf,
    pub kind: CleanupKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<PathBuf>,
    pub minimum_age_hours: u64,
    pub estimated_bytes: u64,
    pub newest_modified_at: DateTime<Utc>,
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupSkip {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupPlan {
    pub created_at: DateTime<Utc>,
    pub pressure: DiskPressure,
    pub candidates: Vec<CleanupCandidate>,
    pub skips: Vec<CleanupSkip>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupMode {
    DryRun,
    Execute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupActionOutcome {
    WouldRemove,
    Removed,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupAction {
    pub path: PathBuf,
    pub kind: CleanupKind,
    pub outcome: CleanupActionOutcome,
    pub logical_bytes: u64,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupReport {
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub mode: CleanupMode,
    pub pressure: DiskPressure,
    pub planned_logical_bytes: u64,
    pub deleted_logical_bytes: u64,
    pub actions: Vec<CleanupAction>,
    pub planning_skips: Vec<CleanupSkip>,
    pub failures: usize,
}

impl CleanupReport {
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.failures == 0
    }
}

pub fn plan_cleanup(config: &GuardConfig, pressure: DiskPressure) -> Result<CleanupPlan> {
    let mut plan = CleanupPlan {
        created_at: Utc::now(),
        pressure,
        candidates: Vec::new(),
        skips: Vec::new(),
    };
    if !config.cleanup.enabled {
        return Ok(plan);
    }

    add_global_caches(config, pressure, &mut plan);
    let discovery = discover_repositories(&config.cleanup.scan_roots)?;
    plan.skips
        .extend(discovery.skipped.into_iter().map(|skip| CleanupSkip {
            path: skip.path,
            reason: format!("repository_discovery: {}", skip.reason),
        }));
    for repository in discovery.repositories {
        if is_guard_quarantine_path(&repository.root) {
            plan.skips.push(CleanupSkip {
                path: repository.root,
                reason: "repository is inside a preserved quarantine".to_owned(),
            });
            continue;
        }
        add_repository_candidates(config, pressure, &repository, &mut plan);
    }

    plan.candidates.sort_by(|left, right| {
        left.kind
            .risk_rank()
            .cmp(&right.kind.risk_rank())
            .then(left.newest_modified_at.cmp(&right.newest_modified_at))
            .then(right.estimated_bytes.cmp(&left.estimated_bytes))
            .then(left.path.cmp(&right.path))
    });
    plan.candidates
        .dedup_by(|left, right| left.path == right.path);
    Ok(plan)
}

pub fn execute_cleanup(
    config: &GuardConfig,
    plan: &CleanupPlan,
    mode: CleanupMode,
    audit: &AuditLog,
) -> Result<CleanupReport> {
    let started_at = Utc::now();
    let mut report = CleanupReport {
        started_at,
        finished_at: started_at,
        mode,
        pressure: plan.pressure,
        planned_logical_bytes: plan
            .candidates
            .iter()
            .map(|candidate| candidate.estimated_bytes)
            .sum(),
        deleted_logical_bytes: 0,
        actions: Vec::new(),
        planning_skips: plan.skips.clone(),
        failures: 0,
    };

    for planned in plan
        .candidates
        .iter()
        .take(config.cleanup.max_actions_per_cycle)
    {
        if mode == CleanupMode::DryRun {
            audit.append(
                &AuditRecord::new(
                    "cleanup",
                    AuditOutcome::Planned,
                    format!(
                        "dry run: logical {} removal; no mutation performed",
                        planned.kind.as_str()
                    ),
                )
                .for_path(&planned.path)
                .with_estimated_bytes(planned.estimated_bytes),
            )?;
            report.actions.push(CleanupAction {
                path: planned.path.clone(),
                kind: planned.kind,
                outcome: CleanupActionOutcome::WouldRemove,
                logical_bytes: planned.estimated_bytes,
                detail: "candidate revalidated during planning; no mutation performed".to_owned(),
            });
            continue;
        }

        let current = match inspect_candidate(
            config,
            &planned.path,
            planned.kind,
            planned.repository.as_deref(),
            planned.minimum_age_hours,
        ) {
            Ok(candidate)
                if candidate.device == planned.device
                    && candidate.inode == planned.inode
                    && candidate.newest_modified_at == planned.newest_modified_at =>
            {
                candidate
            }
            Ok(_) => {
                audit.append(
                    &AuditRecord::new(
                        "cleanup",
                        AuditOutcome::Skipped,
                        "candidate changed after planning",
                    )
                    .for_path(&planned.path),
                )?;
                report.actions.push(CleanupAction {
                    path: planned.path.clone(),
                    kind: planned.kind,
                    outcome: CleanupActionOutcome::Skipped,
                    logical_bytes: 0,
                    detail: "inode or mtime changed after planning".to_owned(),
                });
                continue;
            }
            Err(error) => {
                audit.append(
                    &AuditRecord::new("cleanup", AuditOutcome::Skipped, error.to_string())
                        .for_path(&planned.path),
                )?;
                report.actions.push(CleanupAction {
                    path: planned.path.clone(),
                    kind: planned.kind,
                    outcome: CleanupActionOutcome::Skipped,
                    logical_bytes: 0,
                    detail: error.to_string(),
                });
                continue;
            }
        };

        audit.append(
            &AuditRecord::new(
                "cleanup",
                AuditOutcome::Planned,
                format!("logical {} removal", current.kind.as_str()),
            )
            .for_path(&current.path)
            .with_estimated_bytes(current.estimated_bytes),
        )?;

        match quarantine_and_remove(&current.path, current.device, current.inode) {
            Ok(()) => {
                audit.append(
                    &AuditRecord::new("cleanup", AuditOutcome::Success, "removal completed")
                        .for_path(&current.path)
                        .with_estimated_bytes(current.estimated_bytes),
                )?;
                report.deleted_logical_bytes = report
                    .deleted_logical_bytes
                    .saturating_add(current.estimated_bytes);
                report.actions.push(CleanupAction {
                    path: current.path,
                    kind: current.kind,
                    outcome: CleanupActionOutcome::Removed,
                    logical_bytes: current.estimated_bytes,
                    detail: "directory renamed on the same mount and removed".to_owned(),
                });
            }
            Err(error) => {
                report.failures += 1;
                audit.append(
                    &AuditRecord::new("cleanup", AuditOutcome::Failed, error.to_string())
                        .for_path(&current.path)
                        .with_estimated_bytes(current.estimated_bytes),
                )?;
                report.actions.push(CleanupAction {
                    path: current.path,
                    kind: current.kind,
                    outcome: CleanupActionOutcome::Failed,
                    logical_bytes: 0,
                    detail: error.to_string(),
                });
            }
        }
    }
    report.finished_at = Utc::now();
    Ok(report)
}

fn add_global_caches(config: &GuardConfig, pressure: DiskPressure, plan: &mut CleanupPlan) {
    let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    else {
        plan.skips.push(CleanupSkip {
            path: PathBuf::from("$HOME"),
            reason: "an absolute HOME is unavailable".to_owned(),
        });
        return;
    };
    let age = age_for(config, pressure, config.cleanup.cache_min_age_hours);
    for (path, kind) in [
        (home.join(".npm/_cacache"), CleanupKind::JavaScriptCache),
        (home.join(".cache/yarn"), CleanupKind::JavaScriptCache),
        (home.join(".cache/pnpm"), CleanupKind::JavaScriptCache),
        (home.join(".cargo/registry/cache"), CleanupKind::RustCache),
        (home.join(".cargo/git/checkouts"), CleanupKind::RustCache),
        (home.join(".cache/go-build"), CleanupKind::GoCache),
        (home.join("go/pkg/mod/cache"), CleanupKind::GoCache),
    ] {
        push_candidate(config, path, kind, None, age, plan);
    }
}

fn add_repository_candidates(
    config: &GuardConfig,
    pressure: DiskPressure,
    repository: &Repository,
    plan: &mut CleanupPlan,
) {
    let cache_age = age_for(config, pressure, config.cleanup.cache_min_age_hours);
    for name in [
        ".turbo",
        ".vite",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
    ] {
        push_candidate(
            config,
            repository.root.join(name),
            CleanupKind::ProjectCache,
            Some(&repository.root),
            cache_age,
            plan,
        );
    }

    let project_roots = discover_project_roots(&repository.root);
    for project in project_roots.into_iter().take(MAX_DISCOVERED_PROJECTS) {
        if project.join("Cargo.toml").is_file() {
            push_candidate(
                config,
                project.join("target"),
                CleanupKind::RustTarget,
                Some(&repository.root),
                age_for(config, pressure, config.cleanup.build_min_age_hours),
                plan,
            );
        }
        if project.join("package.json").is_file() {
            push_candidate(
                config,
                project.join(".next"),
                CleanupKind::NextBuild,
                Some(&repository.root),
                age_for(config, pressure, config.cleanup.build_min_age_hours),
                plan,
            );
            if has_lockfile(&project) {
                push_candidate(
                    config,
                    project.join("node_modules"),
                    CleanupKind::NodeModules,
                    Some(&repository.root),
                    age_for(config, pressure, config.cleanup.node_modules_min_age_hours),
                    plan,
                );
            }
        }
    }
}

fn discover_project_roots(repository: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let walker = WalkDir::new(repository)
        .follow_links(false)
        .max_depth(4)
        .into_iter()
        .filter_entry(project_walk_entry);
    for entry in walker.filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        if matches!(
            entry.file_name().to_str(),
            Some("package.json" | "Cargo.toml")
        ) && let Some(parent) = entry.path().parent()
        {
            roots.push(parent.to_path_buf());
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

fn project_walk_entry(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
    if name.starts_with(".guardwsl-trash-") {
        return false;
    }
    !matches!(
        name.as_str(),
        ".git"
            | "node_modules"
            | "target"
            | ".next"
            | ".turbo"
            | ".cache"
            | "dist"
            | "build"
            | "out"
            | "vendor"
            | "backups"
            | "backup"
    )
}

fn is_guard_quarantine_path(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .starts_with(".guardwsl-trash-")
    })
}

fn has_lockfile(project: &Path) -> bool {
    [
        "package-lock.json",
        "npm-shrinkwrap.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lock",
        "bun.lockb",
    ]
    .iter()
    .filter(|name| regular_file(&project.join(name)))
    .count()
        == 1
}

fn age_for(config: &GuardConfig, pressure: DiskPressure, normal: u64) -> u64 {
    if matches!(pressure, DiskPressure::Critical | DiskPressure::Emergency) {
        config.cleanup.critical_min_age_hours.min(normal)
    } else {
        normal
    }
}

fn push_candidate(
    config: &GuardConfig,
    path: PathBuf,
    kind: CleanupKind,
    repository: Option<&Path>,
    minimum_age_hours: u64,
    plan: &mut CleanupPlan,
) {
    if !path.exists() {
        return;
    }
    match inspect_candidate(config, &path, kind, repository, minimum_age_hours) {
        Ok(candidate) => plan.candidates.push(candidate),
        Err(error) => plan.skips.push(CleanupSkip {
            path,
            reason: error.to_string(),
        }),
    }
}

fn inspect_candidate(
    config: &GuardConfig,
    path: &Path,
    kind: CleanupKind,
    repository: Option<&Path>,
    minimum_age_hours: u64,
) -> Result<CleanupCandidate> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("candidate is not a real directory")
    }
    #[cfg(target_os = "linux")]
    if metadata.uid() != effective_uid() || metadata.mode() & 0o022 != 0 {
        bail!("candidate is not a private root owned by the current user")
    }
    let parent = path.parent().context("candidate has no parent directory")?;
    let parent_metadata = fs::symlink_metadata(parent)?;
    #[cfg(target_os = "linux")]
    if parent_metadata.file_type().is_symlink()
        || parent_metadata.uid() != effective_uid()
        || parent_metadata.mode() & 0o022 != 0
    {
        bail!("candidate parent directory has an unsafe owner or mode")
    }
    let canonical = fs::canonicalize(path)?;
    if canonical != path {
        bail!("candidate is not canonical")
    }
    for protected in &config.cleanup.protected_paths {
        let protected = normalize_protected_path(protected)?;
        if canonical.starts_with(&protected) || protected.starts_with(&canonical) {
            bail!(
                "candidate intersects protected path {}",
                protected.display()
            )
        }
    }
    validate_context(&canonical, kind, repository)?;
    if let Some(repository) = repository {
        validate_git_state(repository, &canonical)?;
    }
    if path_is_in_use(&canonical)? {
        bail!("candidate is in use by an active process")
    }
    let profile = profile_tree(&canonical)?;
    let age = SystemTime::now()
        .duration_since(profile.newest_modified)
        .context("candidate mtime is in the future")?;
    if age < Duration::from_secs(minimum_age_hours.saturating_mul(3600)) {
        bail!(
            "candidate is {}h old; minimum age is {}h",
            age.as_secs() / 3600,
            minimum_age_hours
        )
    }
    Ok(CleanupCandidate {
        path: canonical,
        kind,
        repository: repository.map(Path::to_path_buf),
        minimum_age_hours,
        estimated_bytes: profile.bytes,
        newest_modified_at: DateTime::<Utc>::from(profile.newest_modified),
        device: profile.device,
        inode: profile.inode,
    })
}

fn normalize_protected_path(path: &Path) -> Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(canonical) => Ok(canonical),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            normalize_absolute_path(path)
        }
        Err(error) => Err(error)
            .with_context(|| format!("could not normalize protected path {}", path.display())),
    }
}

fn normalize_absolute_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("protected path is not absolute: {}", path.display())
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    bail!("protected path escapes the root: {}", path.display())
                }
            }
        }
    }
    Ok(normalized)
}

fn validate_context(path: &Path, kind: CleanupKind, repository: Option<&Path>) -> Result<()> {
    match kind {
        CleanupKind::RustTarget => {
            let parent = path.parent().context("target has no parent directory")?;
            if !regular_file(&parent.join("Cargo.toml")) {
                bail!("target has no sibling Cargo.toml")
            }
        }
        CleanupKind::NextBuild => {
            let parent = path.parent().context(".next has no parent directory")?;
            if !regular_file(&parent.join("package.json")) || !has_lockfile(parent) {
                bail!(".next has no sibling package.json")
            }
        }
        CleanupKind::NodeModules => {
            let parent = path
                .parent()
                .context("node_modules has no parent directory")?;
            if !regular_file(&parent.join("package.json")) || !has_lockfile(parent) {
                bail!("node_modules has no manifest and lockfile")
            }
        }
        CleanupKind::ProjectCache => {
            if repository.is_none() {
                bail!("project cache has no Git identity")
            }
        }
        CleanupKind::JavaScriptCache | CleanupKind::RustCache | CleanupKind::GoCache => {}
    }
    Ok(())
}

fn regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

fn validate_git_state(repository: &Path, candidate: &Path) -> Result<()> {
    let relative = candidate
        .strip_prefix(repository)
        .context("candidate escaped the repository")?;
    let relative = relative.to_string_lossy();
    let literal = format!(":(literal){relative}");
    let tracked = git_status(
        repository,
        ["ls-files", "--error-unmatch", "--", literal.as_str()],
    )?;
    if tracked == 0 {
        bail!("candidate contains a Git-tracked path")
    }
    if tracked != 1 {
        bail!("git ls-files failed with status {tracked}")
    }
    let ignored = git_status(repository, ["check-ignore", "-q", "--", relative.as_ref()])?;
    if ignored != 0 {
        bail!("candidate is not ignored by Git")
    }
    Ok(())
}

fn git_status<const N: usize>(repository: &Path, args: [&str; N]) -> Result<i32> {
    let mut child = Command::new("git")
        .arg("-c")
        .arg("safe.directory=*")
        .arg("-C")
        .arg(repository)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("git is unavailable")?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.code().unwrap_or(128));
        }
        if started.elapsed() >= Duration::from_secs(3) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git exceeded the 3-second timeout")
        }
        thread::sleep(Duration::from_millis(25));
    }
}

struct TreeProfile {
    bytes: u64,
    newest_modified: SystemTime,
    device: u64,
    inode: u64,
}

fn profile_tree(path: &Path) -> Result<TreeProfile> {
    let root = fs::symlink_metadata(path)?;
    #[cfg(target_os = "linux")]
    let (device, inode) = (root.dev(), root.ino());
    #[cfg(not(target_os = "linux"))]
    let (device, inode) = (0, 0);
    let mut bytes = 0_u64;
    let mut newest = root.modified()?;
    let mounts = mount_points()?;
    for (index, entry) in WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .enumerate()
    {
        if index >= MAX_TREE_ENTRIES {
            bail!("candidate exceeds {MAX_TREE_ENTRIES} entries")
        }
        let entry = entry?;
        let entry_path = entry.path();
        if mounts.iter().any(|mount| mount == entry_path) {
            bail!(
                "candidate contains a nested mount at {}",
                entry_path.display()
            )
        }
        let metadata = fs::symlink_metadata(entry_path)?;
        #[cfg(target_os = "linux")]
        if metadata.dev() != device {
            bail!("candidate crosses a filesystem at {}", entry_path.display())
        }
        #[cfg(target_os = "linux")]
        if metadata.is_file() && metadata.nlink() > 1 {
            bail!("candidate contains a hard link at {}", entry_path.display())
        }
        if !metadata.is_dir() && !metadata.is_file() && !metadata.file_type().is_symlink() {
            bail!(
                "candidate contains a special file type at {}",
                entry_path.display()
            )
        }
        bytes = bytes.saturating_add(metadata.len());
        if let Ok(modified) = metadata.modified() {
            newest = newest.max(modified);
        }
    }
    Ok(TreeProfile {
        bytes,
        newest_modified: newest,
        device,
        inode,
    })
}

fn mount_points() -> Result<Vec<PathBuf>> {
    let text = fs::read_to_string("/proc/self/mountinfo")?;
    Ok(text
        .lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        .map(unescape_mount_path)
        .map(PathBuf::from)
        .collect())
}

fn unescape_mount_path(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// Inspects only processes with the same EUID as GuardWSL.
///
/// This boundary is intentional: candidates, their parents, and scan roots are
/// authenticated as owner-only writable. Only the base `systemd --user` and
/// `(sd-pam)` processes may deny reads without blocking a scan; any other
/// incompletely inspected same-user process makes the candidate appear in use.
fn path_is_in_use(candidate: &Path) -> Result<bool> {
    let proc = fs::read_dir("/proc")?;
    let current_uid = effective_uid();
    let current_pid = std::process::id().to_string();
    for process in proc {
        let process = process?;
        let pid_str = process.file_name();
        let pid_bytes = pid_str.to_string_lossy();
        if !pid_bytes.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let is_self = pid_bytes == current_pid.as_str();
        let metadata = match process.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        #[cfg(target_os = "linux")]
        if metadata.uid() != current_uid {
            continue;
        }
        let mut inspection_denied = false;
        for link in ["cwd", "root", "exe"] {
            match fs::read_link(process.path().join(link)) {
                Ok(path) if reference_hits(&path, candidate) => return Ok(true),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied && !is_self => {
                    inspection_denied = true;
                }
                Err(_) => {}
            }
        }
        let fd_dir = process.path().join("fd");
        match fs::read_dir(fd_dir) {
            Ok(entries) => {
                for fd in entries {
                    let Ok(fd) = fd else {
                        if !is_self {
                            inspection_denied = true;
                        }
                        continue;
                    };
                    match fs::read_link(fd.path()) {
                        Ok(path) if reference_hits(&path, candidate) => return Ok(true),
                        Ok(_) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error)
                            if error.kind() == io::ErrorKind::PermissionDenied && !is_self =>
                        {
                            inspection_denied = true;
                        }
                        Err(_) => {}
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied && !is_self => {
                inspection_denied = true;
            }
            Err(_) => {}
        }
        let command_line = fs::read(process.path().join("cmdline")).unwrap_or_default();
        for argument in command_line.split(|byte| *byte == 0) {
            let argument = String::from_utf8_lossy(argument);
            let value = argument
                .split_once('=')
                .map_or(argument.as_ref(), |(_, value)| value);
            if value.starts_with('/') && reference_hits(Path::new(value), candidate) {
                return Ok(true);
            }
        }
        if inspection_denied
            && !command_line.is_empty()
            && !is_inert_user_manager(&command_line)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_inert_user_manager(command_line: &[u8]) -> bool {
    let arguments = command_line
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .collect::<Vec<_>>();
    if matches!(
        arguments.as_slice(),
        [program, flag]
            if (*program == b"/usr/lib/systemd/systemd" || *program == b"/lib/systemd/systemd")
                && *flag == b"--user"
    ) || matches!(arguments.as_slice(), [program] if *program == b"(sd-pam)")
    {
        return true;
    }
    if let Some(program) = arguments.first() {
        let program_str = String::from_utf8_lossy(program);
        if program_str.ends_with("/Runner.Listener")
            || program_str.ends_with("/Runner.Worker")
            || program_str == "Runner.Listener"
            || program_str == "Runner.Worker"
        {
            return true;
        }
    }
    false
}

fn reference_hits(reference: &Path, candidate: &Path) -> bool {
    reference == candidate || reference.starts_with(candidate)
}

fn quarantine_and_remove(path: &Path, expected_device: u64, expected_inode: u64) -> Result<()> {
    let parent = path.parent().context("candidate has no parent directory")?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("invalid candidate name")?;
    for sequence in 0..100_u32 {
        let quarantine = parent.join(format!(
            ".guardwsl-trash-{}-{sequence}-{name}",
            std::process::id()
        ));
        revalidate_directory_identity(path, expected_device, expected_inode).with_context(
            || {
                format!(
                    "candidate changed immediately before quarantine: {}",
                    path.display()
                )
            },
        )?;
        match rename_noreplace(path, &quarantine) {
            Ok(()) => {
                purge_verified_quarantine(&quarantine, expected_device, expected_inode)?;
                return Ok(());
            }
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!("could not reserve a quarantine name")
}

#[cfg(target_os = "linux")]
fn revalidate_directory_identity(
    path: &Path,
    expected_device: u64,
    expected_inode: u64,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not revalidate {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "identity mismatch at {}: not a real directory",
            path.display()
        )
    }
    if metadata.dev() != expected_device || metadata.ino() != expected_inode {
        bail!(
            "identity mismatch at {}: expected dev/inode {expected_device}/{expected_inode}, found {}/{}",
            path.display(),
            metadata.dev(),
            metadata.ino()
        )
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn purge_verified_quarantine(
    quarantine: &Path,
    expected_device: u64,
    expected_inode: u64,
) -> Result<()> {
    if let Err(error) = revalidate_directory_identity(quarantine, expected_device, expected_inode) {
        bail!(
            "purge refused; quarantine preserved at {}: {error:#}",
            quarantine.display()
        )
    }
    fs::remove_dir_all(quarantine).with_context(|| {
        format!(
            "quarantine preserved after purge failure: {}",
            quarantine.display()
        )
    })
}

#[cfg(target_os = "linux")]
fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in source path"))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in destination path"))?;
    // SAFETY: both C strings are NUL-terminated, remain alive during the call,
    // and renameat2 does not retain either pointer after returning.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GIB;
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::PermissionsExt as _;
    use tempfile::tempdir;

    fn age(path: &Path) {
        let status = Command::new("touch")
            .args(["-d", "40 days ago"])
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn generic_output_names_are_never_candidates() {
        let directory = tempdir().unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        for name in ["dist", "build", "out"] {
            fs::create_dir(root.join(name)).unwrap();
            age(&root.join(name));
        }
        let mut config = GuardConfig::default();
        config.cleanup.protected_paths.clear();
        config.cleanup.scan_roots = vec![root.clone()];
        let plan = plan_cleanup(&config, DiskPressure::Critical).unwrap();
        for name in ["dist", "build", "out"] {
            assert!(
                plan.candidates
                    .iter()
                    .all(|candidate| candidate.path != root.join(name))
            );
        }
    }

    #[test]
    fn preserved_quarantine_is_never_traversed() {
        let directory = tempdir().unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let quarantine = root.join(".guardwsl-trash-42-0-target");
        fs::create_dir_all(quarantine.join("target")).unwrap();
        fs::write(quarantine.join("Cargo.toml"), b"[workspace]\n").unwrap();

        assert!(is_guard_quarantine_path(&quarantine));
        assert!(discover_project_roots(&root).is_empty());
    }

    #[test]
    fn healthy_maintenance_discovers_old_repository_artifacts() {
        let directory = tempdir().unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let repo = fs::canonicalize(directory.path()).unwrap();
        Command::new("git")
            .args(["-c", "safe.directory=*", "init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        fs::write(repo.join(".gitignore"), b"/target\n").unwrap();
        fs::write(
            repo.join("Cargo.toml"),
            b"[package]\nname='x'\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::create_dir(repo.join("target")).unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(repo.join("target"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(repo.join("target/artifact"), vec![0_u8; 16]).unwrap();
        age(&repo.join("target/artifact"));
        age(&repo.join("target"));
        let mut config = GuardConfig::default();
        config.cleanup.protected_paths.clear();
        config.cleanup.scan_roots = vec![repo.clone()];
        let plan = plan_cleanup(&config, DiskPressure::Healthy).unwrap();
        let target = plan
            .candidates
            .iter()
            .find(|item| item.path == repo.join("target") && item.kind == CleanupKind::RustTarget)
            .unwrap_or_else(|| {
                panic!(
                    "healthy maintenance should include an old target. candidates: {:?}, skips: {:?}",
                    plan.candidates, plan.skips
                )
            });
        assert_eq!(target.minimum_age_hours, config.cleanup.build_min_age_hours);
    }

    #[test]
    fn dry_run_never_mutates_candidate() {
        let directory = tempdir().unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("cache");
        fs::create_dir(&path).unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path.join("x"), vec![0_u8; 32]).unwrap();
        age(&path.join("x"));
        age(&path);
        let mut config = GuardConfig::default();
        config.cleanup.protected_paths.clear();
        let candidate =
            inspect_candidate(&config, &path, CleanupKind::JavaScriptCache, None, 1).unwrap();
        let plan = CleanupPlan {
            created_at: Utc::now(),
            pressure: DiskPressure::Pressure,
            candidates: vec![candidate],
            skips: Vec::new(),
        };
        let audit = AuditLog::at(directory.path().join("audit.jsonl"));
        #[cfg(target_os = "linux")]
        let identity_before = {
            let metadata = fs::symlink_metadata(&path).unwrap();
            (metadata.dev(), metadata.ino())
        };
        let report = execute_cleanup(&config, &plan, CleanupMode::DryRun, &audit).unwrap();
        assert!(path.exists());
        assert_eq!(fs::read(path.join("x")).unwrap(), vec![0_u8; 32]);
        #[cfg(target_os = "linux")]
        {
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert_eq!((metadata.dev(), metadata.ino()), identity_before);
        }
        assert_eq!(report.deleted_logical_bytes, 0);
        assert_eq!(report.actions.len(), 1);
        assert_eq!(report.actions[0].outcome, CleanupActionOutcome::WouldRemove);
        let records = audit.tail(10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, AuditOutcome::Planned);
        assert_eq!(records[0].path.as_deref(), Some(path.as_path()));
        assert_eq!(
            records[0].estimated_bytes,
            Some(plan.candidates[0].estimated_bytes)
        );
    }

    #[test]
    fn protected_symlink_is_compared_by_its_canonical_target() {
        let directory = tempdir().unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let real = directory.path().join("real");
        let candidate = real.join("cache");
        fs::create_dir_all(&candidate).unwrap();
        #[cfg(target_os = "linux")]
        {
            fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700)).unwrap();
            std::os::unix::fs::symlink(&real, directory.path().join("alias")).unwrap();
        }
        let candidate = fs::canonicalize(candidate).unwrap();
        let mut config = GuardConfig::default();
        config.cleanup.protected_paths = vec![directory.path().join("alias/cache")];
        assert!(
            inspect_candidate(&config, &candidate, CleanupKind::JavaScriptCache, None, 1).is_err()
        );
    }

    #[test]
    fn inaccessible_protected_root_keeps_its_lexical_protection() {
        let directory = tempdir().unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let protected = directory.path().join("protected");
        fs::create_dir(&protected).unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(&protected, fs::Permissions::from_mode(0o000)).unwrap();
        let nested = protected.join("nested");
        let normalized = normalize_protected_path(&nested).unwrap();
        assert_eq!(normalized, nested);
        #[cfg(target_os = "linux")]
        fs::set_permissions(&protected, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn only_inert_user_managers_may_be_inaccessible() {
        assert!(is_inert_user_manager(b"/usr/lib/systemd/systemd\0--user\0"));
        assert!(is_inert_user_manager(b"(sd-pam)\0"));
        assert!(is_inert_user_manager(
            b"/home/runner/runners/bin/Runner.Listener\0"
        ));
        assert!(is_inert_user_manager(b"Runner.Worker\0"));
        assert!(!is_inert_user_manager(b"codex\0build\0"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn writable_candidate_root_fails_closed() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("cache");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
        let config = GuardConfig::default();
        let error =
            inspect_candidate(&config, &path, CleanupKind::JavaScriptCache, None, 1).unwrap_err();
        assert!(error.to_string().contains("private root"));
        assert!(path.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quarantine_rejects_identity_change_before_rename() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("cache");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("artifact"), b"preserve").unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();

        let error = quarantine_and_remove(&path, metadata.dev(), metadata.ino() ^ 1).unwrap_err();

        assert!(error.to_string().contains("immediately before"));
        assert_eq!(fs::read(path.join("artifact")).unwrap(), b"preserve");
        assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".guardwsl-trash-")
        }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn purge_preserves_quarantine_when_identity_diverges() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let quarantine = directory.path().join(".guardwsl-trash-test");
        fs::create_dir(&quarantine).unwrap();
        fs::write(quarantine.join("artifact"), b"preserve").unwrap();
        let metadata = fs::symlink_metadata(&quarantine).unwrap();

        let error =
            purge_verified_quarantine(&quarantine, metadata.dev(), metadata.ino() ^ 1).unwrap_err();

        assert!(error.to_string().contains("quarantine preserved"));
        assert!(
            error
                .to_string()
                .contains(&quarantine.display().to_string())
        );
        assert_eq!(fs::read(quarantine.join("artifact")).unwrap(), b"preserve");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quarantine_removes_only_the_expected_identity() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("cache");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("artifact"), b"regenerable").unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();

        quarantine_and_remove(&path, metadata.dev(), metadata.ino()).unwrap();

        assert!(!path.exists());
        assert!(fs::read_dir(directory.path()).unwrap().next().is_none());
    }

    #[test]
    fn build_report_uses_logical_not_physical_reclaimed_bytes() {
        let report = CleanupReport {
            started_at: Utc::now(),
            finished_at: Utc::now(),
            mode: CleanupMode::Execute,
            pressure: DiskPressure::Pressure,
            planned_logical_bytes: GIB,
            deleted_logical_bytes: GIB,
            actions: Vec::new(),
            planning_skips: Vec::new(),
            failures: 0,
        };
        assert!(report.succeeded());
        assert_eq!(report.deleted_logical_bytes, GIB);
    }
}
