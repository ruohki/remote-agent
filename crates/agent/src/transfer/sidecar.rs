//! Partial-file bookkeeping: `<name>.part` + `<name>.part.json` sidecars that make uploads
//! resumable across sessions, safe file naming, and cleanup of abandoned partials.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const PART_SUFFIX: &str = ".part";
pub const SIDECAR_SUFFIX: &str = ".part.json";
/// Partials older than this are deleted by [`cleanup_stale`].
pub const STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Sidecar {
    pub token: String,
    pub name: String,
    pub size: u64,
    pub received: u64,
}

impl Sidecar {
    pub fn path_for(part: &Path) -> PathBuf {
        let mut s = part.as_os_str().to_owned();
        s.push(".json");
        PathBuf::from(s)
    }

    pub fn write(&self, part: &Path) -> Result<()> {
        let path = Self::path_for(part);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("renaming {}", tmp.display()))?;
        Ok(())
    }

    pub fn read(part: &Path) -> Result<Self> {
        let path = Self::path_for(part);
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn remove(part: &Path) {
        let _ = std::fs::remove_file(Self::path_for(part));
    }
}

/// Strip any directory components and reject names that could escape the target directory.
pub fn safe_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('.')
        .to_string();
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

/// `name.ext` → `name (2).ext`, `name (3).ext`, … until no file/partial with that name exists.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    unique_path_ignoring(dir, name, None)
}

/// Final destination for a completed partial: the name it was reserved under (the `.part`
/// stripped), unless something else appeared there meanwhile.
pub fn final_path_for(part: &Path, dir: &Path, name: &str) -> PathBuf {
    let reserved = part
        .to_str()
        .and_then(|s| s.strip_suffix(PART_SUFFIX))
        .map(PathBuf::from);
    match reserved {
        Some(p) if !p.exists() => p,
        _ => unique_path_ignoring(dir, name, Some(part)),
    }
}

/// Like [`unique_path`] but treats `ignore` (our own partial) as absent.
pub fn unique_path_ignoring(dir: &Path, name: &str, ignore: Option<&Path>) -> PathBuf {
    let exists_any = |candidate: &Path| -> bool {
        if candidate.exists() {
            return true;
        }
        let mut part = candidate.as_os_str().to_owned();
        part.push(PART_SUFFIX);
        let part = PathBuf::from(part);
        if ignore.map(|i| i == part).unwrap_or(false) {
            return false;
        }
        part.exists()
    };
    let candidate = dir.join(name);
    if !exists_any(&candidate) {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for n in 2.. {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !exists_any(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// Look for a partial in `dir` created for `token`. Returns the `.part` path and how many
/// bytes it verifiably holds (min of sidecar count and file length).
pub fn find_resume(dir: &Path, token: &str) -> Option<(PathBuf, Sidecar)> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name()?.to_string_lossy().into_owned();
        let Some(part_name) = name.strip_suffix(".json") else {
            continue;
        };
        if !part_name.ends_with(PART_SUFFIX) {
            continue;
        }
        let part = dir.join(part_name);
        let Ok(mut sidecar) = Sidecar::read(&part) else {
            continue;
        };
        if sidecar.token != token {
            continue;
        }
        let len = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        sidecar.received = sidecar.received.min(len);
        return Some((part, sidecar));
    }
    None
}

/// Delete `.part` files (and their sidecars) not modified for [`STALE_AFTER`].
pub fn cleanup_stale(dir: &Path, max_age: Duration) -> usize {
    let mut removed = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let Some(name) = name else { continue };
        if !name.ends_with(PART_SUFFIX) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .map(|age| age > max_age)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(&path);
            Sidecar::remove(&path);
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("C:\\Users\\x\\doc.txt"), "doc.txt");
        assert_eq!(safe_name("..."), "file");
        assert_eq!(safe_name("re:port?.pdf"), "report.pdf");
        assert_eq!(safe_name("  photo.jpg "), "photo.jpg");
    }

    #[test]
    fn unique_and_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        assert_eq!(unique_path(dir, "a.txt"), dir.join("a (2).txt"));
        std::fs::write(dir.join("a (2).txt.part"), b"x").unwrap();
        assert_eq!(unique_path(dir, "a.txt"), dir.join("a (3).txt"));
        assert_eq!(unique_path(dir, "noext"), dir.join("noext"));
        // A completed partial lands under its reserved name, not a "(n)" variant.
        let part = dir.join("a (3).txt.part");
        std::fs::write(&part, b"x").unwrap();
        assert_eq!(final_path_for(&part, dir, "a.txt"), dir.join("a (3).txt"));
        std::fs::write(dir.join("a (3).txt"), b"taken").unwrap();
        assert_eq!(final_path_for(&part, dir, "a.txt"), dir.join("a (4).txt"));
        std::fs::remove_file(&part).unwrap();
        std::fs::remove_file(dir.join("a (3).txt")).unwrap();

        let part = dir.join("big.bin.part");
        std::fs::write(&part, vec![0u8; 100]).unwrap();
        Sidecar {
            token: "tok".into(),
            name: "big.bin".into(),
            size: 1000,
            received: 150,
        }
        .write(&part)
        .unwrap();
        let (found, sc) = find_resume(dir, "tok").unwrap();
        assert_eq!(found, part);
        assert_eq!(sc.received, 100, "clamped to the file length");
        assert!(find_resume(dir, "other").is_none());

        assert_eq!(cleanup_stale(dir, Duration::from_secs(3600)), 0);
        assert_eq!(cleanup_stale(dir, Duration::ZERO), 2);
        assert!(!part.exists());
        assert!(!Sidecar::path_for(&part).exists());
    }
}
