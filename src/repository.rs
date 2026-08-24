//! Bounded repository discovery under explicit roots.

use crate::fsutil::effective_uid;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use walkdir::{DirEntry, WalkDir};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repository {
    pub root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoverySkip {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryReport {
    pub repositories: Vec<Repository>,
    pub skipped: Vec<DiscoverySkip>,
}

pub fn discover_repositories(roots: &[PathBuf]) -> Result<DiscoveryReport> {
    let roots = authenticate_roots(roots)?;
    let mut report = DiscoveryReport::default();
    for root in &roots {
        let walker = WalkDir::new(root)
            .follow_links(false)
            .max_depth(4)
            .into_iter()
            .filter_entry(should_descend);
        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    report.skipped.push(DiscoverySkip {
                        path: error
                            .path()
                            .map(Path::to_path_buf)
                            .unwrap_or_else(|| root.clone()),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            if !entry.file_type().is_dir() {
                continue;
            }
            let candidate = entry.path();
            let marker = candidate.join(".git");
            if !marker.exists() {
                continue;
            }
            match authenticate_repository(candidate, &marker, &roots) {
                Ok(repository) => report.repositories.push(repository),
                Err(error) => report.skipped.push(DiscoverySkip {
                    path: candidate.to_path_buf(),
                    reason: error.to_string(),
                }),
            }
        }
    }
    report
        .repositories
        .sort_by(|left, right| left.root.cmp(&right.root));
    report
        .repositories
        .dedup_by(|left, right| left.root == right.root);
    Ok(report)
}

fn authenticate_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if roots.is_empty() {
        bail!("no scan root is configured")
    }
    roots
        .iter()
        .map(|root| {
            let metadata = std::fs::symlink_metadata(root)
                .with_context(|| format!("scan root is missing: {}", root.display()))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("unsafe scan root: {}", root.display())
            }
            #[cfg(target_os = "linux")]
            if metadata.uid() != effective_uid() || metadata.permissions().mode() & 0o022 != 0 {
                bail!("unsafe owner or mode on scan root: {}", root.display())
            }
            let canonical = std::fs::canonicalize(root)?;
            if canonical != *root {
                bail!("scan root must be canonical: {}", root.display())
            }
            Ok(canonical)
        })
        .collect()
}

fn authenticate_repository(
    candidate: &Path,
    marker: &Path,
    roots: &[PathBuf],
) -> Result<Repository> {
    let root_metadata = std::fs::symlink_metadata(candidate)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        bail!("repository is not a real directory")
    }
    let canonical = std::fs::canonicalize(candidate)?;
    if canonical != candidate || !roots.iter().any(|root| canonical.starts_with(root)) {
        bail!("repository escaped the scan root")
    }
    let marker_metadata = std::fs::symlink_metadata(marker)?;
    if marker_metadata.file_type().is_symlink() {
        bail!(".git cannot be a symlink")
    }
    if marker_metadata.is_file() {
        if marker_metadata.len() > 4096 {
            bail!(".git file exceeds 4 KiB")
        }
        let text = std::fs::read_to_string(marker)?;
        let target = text
            .trim()
            .strip_prefix("gitdir:")
            .context("invalid .git file")?
            .trim();
        let target = Path::new(target);
        let target = if target.is_absolute() {
            target.to_path_buf()
        } else {
            candidate.join(target)
        };
        let target = std::fs::canonicalize(target)?;
        if !target.join("HEAD").is_file() {
            bail!("gitdir has no HEAD")
        }
    } else if !marker_metadata.is_dir() || !marker.join("HEAD").is_file() {
        bail!("invalid .git directory")
    }
    Ok(Repository { root: canonical })
}

fn should_descend(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
    !matches!(
        name.as_str(),
        ".git"
            | "node_modules"
            | "target"
            | ".next"
            | ".cache"
            | ".venv"
            | "venv"
            | "vendor"
            | "backups"
            | "backup"
            | ".codex"
            | ".claude"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discovers_normal_repository_and_skips_nested_artifacts() {
        let directory = tempdir().unwrap();
        #[cfg(target_os = "linux")]
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(repo.join("node_modules/fake/.git")).unwrap();
        std::fs::write(repo.join("node_modules/fake/.git/HEAD"), b"x").unwrap();
        let report = discover_repositories(&[root]).unwrap();
        assert_eq!(report.repositories, vec![Repository { root: repo }]);
    }

    #[test]
    fn symlink_scan_root_is_rejected() {
        let directory = tempdir().unwrap();
        let link = directory.path().with_extension("link");
        std::os::unix::fs::symlink(directory.path(), &link).unwrap();
        assert!(discover_repositories(std::slice::from_ref(&link)).is_err());
        std::fs::remove_file(link).unwrap();
    }
}
