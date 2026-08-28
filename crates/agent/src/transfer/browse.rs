//! Remote file browser operations (`list` / `mkdir` / `delete` / `rename`).
//!
//! Every path the operator supplies is normalised with [`resolve`]: it must be absolute,
//! must not contain `..`, and — when it exists — its canonical form must stay under the
//! canonical form of its parent (so a symlink cannot be used to reach outside the tree the
//! operator is looking at).

use anyhow::{bail, Context, Result};
use protocol::files::FileEntry;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Well-known starting points offered when `list` has no path.
pub fn roots(transfer_dir: &Path) -> Vec<FileEntry> {
    let mut out = Vec::new();
    let mut push = |name: &str, p: PathBuf| {
        if p.is_dir() {
            out.push(FileEntry {
                name: name.to_string(),
                is_dir: true,
                size: 0,
                modified_ms: None,
                hidden: false,
                path: Some(p.display().to_string()),
            });
        }
    };
    if let Some(home) = home_dir() {
        push("Home", home.clone());
        for sub in ["Desktop", "Documents", "Downloads"] {
            push(sub, home.join(sub));
        }
    }
    push("Transfers", transfer_dir.to_path_buf());
    #[cfg(target_os = "macos")]
    {
        if let Ok(rd) = std::fs::read_dir("/Volumes") {
            for e in rd.flatten() {
                let p = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                push(&name, p);
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        for letter in b'C'..=b'Z' {
            let p = PathBuf::from(format!("{}:\\", letter as char));
            let name = format!("{}:", letter as char);
            push(&name, p);
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        push("/", PathBuf::from("/"));
    }
    out
}

pub fn home_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|u| u.home_dir().to_path_buf())
}

/// Validate and normalise an operator-supplied path.
pub fn resolve(raw: &str) -> Result<PathBuf> {
    let path = PathBuf::from(raw.trim());
    if !path.is_absolute() {
        bail!("path must be absolute");
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("path must not contain '..'");
    }
    if let Some(parent) = path.parent() {
        if parent.exists() && path.exists() {
            let canon_parent = parent.canonicalize().context("canonicalizing parent")?;
            let canon = path.canonicalize().context("canonicalizing path")?;
            if !canon.starts_with(&canon_parent) {
                bail!("path resolves outside of its directory");
            }
        }
    }
    Ok(path)
}

pub fn list(path: &Path) -> Result<Vec<FileEntry>> {
    let rd = std::fs::read_dir(path).with_context(|| format!("reading {}", path.display()))?;
    let mut entries = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let meta = match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified_ms = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64);
        entries.push(FileEntry {
            hidden: name.starts_with('.') || is_hidden(&e.path()),
            name,
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified_ms,
            path: None,
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

#[cfg(target_os = "windows")]
fn is_hidden(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    std::fs::metadata(path)
        .map(|m| m.file_attributes() & 0x2 != 0)
        .unwrap_or(false)
}

#[cfg(not(target_os = "windows"))]
fn is_hidden(_path: &Path) -> bool {
    false
}

pub fn mkdir(path: &Path) -> Result<()> {
    std::fs::create_dir(path).with_context(|| format!("creating {}", path.display()))
}

/// Delete a file or an *empty* directory.
pub fn delete(path: &Path) -> Result<()> {
    let meta =
        std::fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.is_dir() {
        std::fs::remove_dir(path)
            .with_context(|| format!("removing directory {} (must be empty)", path.display()))
    } else {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))
    }
}

pub fn rename(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        bail!("{} already exists", to.display());
    }
    std::fs::rename(from, to)
        .with_context(|| format!("renaming {} → {}", from.display(), to.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_relative_and_parent_components() {
        assert!(resolve("relative/path").is_err());
        assert!(resolve("/tmp/../etc").is_err());
        assert!(resolve("/tmp").is_ok());
    }

    #[test]
    fn listing_and_ops() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        std::fs::write(dir.join("b.txt"), b"hello").unwrap();
        std::fs::create_dir(dir.join("a_dir")).unwrap();
        let entries = list(&dir).unwrap();
        assert_eq!(entries[0].name, "a_dir");
        assert!(entries[0].is_dir);
        assert_eq!(entries[1].name, "b.txt");
        assert_eq!(entries[1].size, 5);

        let new_dir = dir.join("made");
        mkdir(&new_dir).unwrap();
        rename(&new_dir, &dir.join("renamed")).unwrap();
        assert!(dir.join("renamed").is_dir());
        delete(&dir.join("renamed")).unwrap();
        delete(&dir.join("b.txt")).unwrap();
        // non-empty directory refuses
        std::fs::create_dir_all(dir.join("full/child")).unwrap();
        assert!(delete(&dir.join("full")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.join("link")).unwrap();
        assert!(resolve(dir.join("link").to_str().unwrap()).is_err());
    }
}
