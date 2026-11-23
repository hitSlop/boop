use anyhow::Result;
use ignore::WalkBuilder;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

/// Hardcoded secret & local-only patterns that MUST NEVER be synced to remote workers.
pub const MANDATORY_EXCLUDES: &[&str] = &[
    ".git",
    ".boop/",
    ".build/",
    "DerivedData/",
    ".DS_Store",
    ".env",
    ".env.*",
    "*.p8",
    "*.p12",
    "*.pem",
    "*.key",
    "*.mobileprovision",
    "*.provisionprofile",
    "id_rsa*",
    "id_ed25519*",
    // Never delete remote xtool build caches on incremental rsync!
    "xtool/",
    ".bsp/",
];

#[derive(Debug, Clone)]
pub struct SnapshotInfo {
    pub worktree_id: String,
    pub digest: String,
    pub file_count: usize,
    pub excludes: Vec<String>,
}

/// Deterministic 12-hex-char worktree identity based on canonical worktree path.
pub fn compute_worktree_id(root: &Path) -> Result<String> {
    let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let full = hex::encode(hasher.finalize());
    Ok(full[..12].to_string())
}

/// Combine mandatory secret/cache exclusions with project-specific exclusions.
pub fn build_excludes(project_excludes: &[String]) -> Vec<String> {
    let mut out: Vec<String> = MANDATORY_EXCLUDES.iter().map(|s| s.to_string()).collect();
    for pat in project_excludes {
        if !out.iter().any(|existing| existing == pat) {
            out.push(pat.clone());
        }
    }
    out
}

/// Returns true if a relative path matches any mandatory secret/bloat rule or project exclude prefix.
pub fn is_excluded_rel(rel: &str, project_excludes: &[String]) -> bool {
    let rel_norm = rel.replace('\\', "/");
    let parts: Vec<&str> = rel_norm.split('/').collect();
    let file_name = parts.last().copied().unwrap_or("");

    // Hard secret checks (always enforced regardless of .gitignore)
    if file_name == ".env"
        || file_name.starts_with(".env.")
        || file_name == ".DS_Store"
        || file_name.ends_with(".p8")
        || file_name.ends_with(".p12")
        || file_name.ends_with(".pem")
        || file_name.ends_with(".key")
        || file_name.ends_with(".mobileprovision")
        || file_name.ends_with(".provisionprofile")
        || file_name.starts_with("id_rsa")
        || file_name.starts_with("id_ed25519")
    {
        return true;
    }

    for seg in &parts {
        if matches!(
            *seg,
            ".git" | ".boop" | ".build" | "DerivedData" | "xtool" | ".bsp"
        ) {
            return true;
        }
    }

    for pat in project_excludes {
        let trimmed = pat.trim_end_matches('/');
        if trimmed.is_empty() {
            continue;
        }
        if rel_norm == trimmed
            || rel_norm.starts_with(&format!("{trimmed}/"))
            || parts.contains(&trimmed)
        {
            return true;
        }
    }

    false
}

/// Compute an immutable content digest over all synced files (tracked, dirty, and untracked non-ignored).
pub fn compute_snapshot(root: &Path, project_excludes: &[String]) -> Result<SnapshotInfo> {
    let worktree_id = compute_worktree_id(root)?;
    let excludes = build_excludes(project_excludes);

    let mut entries: Vec<(String, u64, i64)> = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .build();

    for entry in walker.flatten() {
        let path = entry.path();
        if path == root || !path.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if is_excluded_rel(&rel_str, project_excludes) {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            let len = meta.len();
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            entries.push((rel_str, len, mtime));
        }
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    for (rel, len, mtime) in &entries {
        hasher.update(rel.as_bytes());
        hasher.update(b":");
        hasher.update(len.to_le_bytes());
        hasher.update(mtime.to_le_bytes());
        hasher.update(b"\n");
    }
    let digest = hex::encode(hasher.finalize())[..16].to_string();

    Ok(SnapshotInfo {
        worktree_id,
        digest,
        file_count: entries.len(),
        excludes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn excludes_secrets_and_includes_untracked_swift() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("App.swift"), "struct App {}").unwrap();
        fs::write(dir.path().join(".env"), "SECRET=123").unwrap();
        fs::write(dir.path().join(".env.local"), "SECRET=456").unwrap();
        fs::write(dir.path().join("AuthKey.p8"), "PRIVATE KEY").unwrap();
        fs::write(dir.path().join("dev.mobileprovision"), "PROFILE").unwrap();
        fs::create_dir_all(dir.path().join("_docs")).unwrap();
        fs::write(dir.path().join("_docs/big.md"), "docs").unwrap();

        let snap1 = compute_snapshot(dir.path(), &["_docs/".into()]).unwrap();
        assert_eq!(snap1.file_count, 1);

        // Adding a new untracked Swift file changes digest and count
        fs::write(dir.path().join("Splash.swift"), "struct Splash {}").unwrap();
        let snap2 = compute_snapshot(dir.path(), &["_docs/".into()]).unwrap();
        assert_eq!(snap2.file_count, 2);
        assert_ne!(snap1.digest, snap2.digest);

        // Adding another secret file does NOT affect snapshot
        fs::write(dir.path().join("prod.key"), "KEY").unwrap();
        let snap3 = compute_snapshot(dir.path(), &["_docs/".into()]).unwrap();
        assert_eq!(snap3.file_count, 2);
        assert_eq!(snap2.digest, snap3.digest);
    }
}
