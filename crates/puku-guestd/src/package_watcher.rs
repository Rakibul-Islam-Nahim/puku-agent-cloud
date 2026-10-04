//! In-guest package-watcher.
//!
//! Per docs/RELIABILITY-REBUILD.md §4.6. Watches the package databases on
//! the guest filesystem (apt, pip, npm, cargo) for changes, and emits an
//! `InstalledPackage` record over vsock to the worker. The worker forwards
//! to controld, which persists to `installed_packages` for later replay by
//! puku-rebuild.
//!
//! For now this is a polling implementation: every minute, read the most
//! recent timestamp from each package database's last-modified directory.
//! A real implementation would use filesystem watchers (inotify); that's
//! out of scope for the wire format this module exposes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// One installed package, as seen by the watcher.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPackage {
    pub kind: PackageKind,
    pub name: String,
    pub version: Option<String>,
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PackageKind {
    Apt,
    Pip,
    Npm,
    Cargo,
    Go,
    Gem,
    Brew,
    System,
}

impl PackageKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Apt => "apt",
            Self::Pip => "pip",
            Self::Npm => "npm",
            Self::Cargo => "cargo",
            Self::Go => "go",
            Self::Gem => "gem",
            Self::Brew => "brew",
            Self::System => "system",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "apt" => Some(Self::Apt),
            "pip" => Some(Self::Pip),
            "npm" => Some(Self::Npm),
            "cargo" => Some(Self::Cargo),
            "go" => Some(Self::Go),
            "gem" => Some(Self::Gem),
            "brew" => Some(Self::Brew),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

/// Determine the kind for a given source path. Returns None when the
/// path doesn't look like any known package database.
pub fn classify(path: &Path) -> Option<PackageKind> {
    let s = path.to_string_lossy();
    if s.contains("/dpkg/status") || s.contains("/var/lib/apt") {
        Some(PackageKind::Apt)
    } else if s.contains("/pip/") || s.contains("/site-packages/") {
        Some(PackageKind::Pip)
    } else if s.contains("/npm/") || s.contains("/node_modules/") {
        Some(PackageKind::Npm)
    } else if s.contains("/.cargo/") {
        Some(PackageKind::Cargo)
    } else if s.contains("/go/pkg/mod/") {
        Some(PackageKind::Go)
    } else if s.contains("/gems/") {
        Some(PackageKind::Gem)
    } else if s.contains("/Homebrew/") {
        Some(PackageKind::Brew)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn classify_recognizes_known_paths() {
        assert_eq!(
            classify(&PathBuf::from("/var/lib/dpkg/status")),
            Some(PackageKind::Apt)
        );
        assert_eq!(
            classify(&PathBuf::from("/usr/lib/python3.11/site-packages/requests")),
            Some(PackageKind::Pip)
        );
        assert_eq!(
            classify(&PathBuf::from("/root/.cargo/registry/src/foo/bar")),
            Some(PackageKind::Cargo)
        );
        assert_eq!(classify(&PathBuf::from("/tmp/random")), None);
    }

    #[test]
    fn kind_strings_round_trip() {
        for k in [
            PackageKind::Apt,
            PackageKind::Pip,
            PackageKind::Npm,
            PackageKind::Cargo,
            PackageKind::Go,
            PackageKind::Gem,
            PackageKind::Brew,
            PackageKind::System,
        ] {
            assert_eq!(PackageKind::parse(k.as_str()), Some(k));
        }
    }
}