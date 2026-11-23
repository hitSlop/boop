use crate::config::state_dir;
use anyhow::Context;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JobRecord {
    pub stage: String,
    pub failure_kind: Option<String>,
    pub retryable: bool,
    pub exit_code: i32,
    pub log_path: Option<PathBuf>,
    pub queue_ms: u64,
    pub fork_ms: Option<u64>,
    pub bootstrap_ms: u64,
    pub sandbox_id: Option<String>,
    pub build_dir_warm: Option<bool>,
    pub dependency_cache_warm: Option<bool>,
    pub remote_workspace: Option<String>,
    #[serde(flatten)]
    pub details: std::collections::BTreeMap<String, serde_json::Value>,
    pub id: String,
    pub command: String,
    pub project: String,
    pub worktree_id: String,
    pub snapshot_digest: String,
    pub worker: String,
    pub status: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub sync_ms: u64,
    pub build_ms: u64,
    pub pull_ms: u64,
    pub sign_ms: u64,
    pub package_ms: u64,
    pub upload_ms: u64,
    pub install_ms: u64,
    pub total_ms: u64,
    pub estimated_cost_usd: f64,
    pub artifact_path: Option<String>,
    pub device: Option<String>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobStore {
    pub jobs: Vec<JobRecord>,
}

pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().context("missing parent directory")?;
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    Ok(())
}

impl JobRecord {
    pub fn new(command: &str) -> Self {
        Self {
            id: format!("job-{}", uuid::Uuid::new_v4().simple()),
            command: command.into(),
            stage: "config".into(),
            started_at: Utc::now(),
            ..Default::default()
        }
    }

    pub fn open_log(&mut self) -> Result<()> {
        let dir = state_dir()?.join("logs");
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.log", self.id));
        fs::File::create_new(&path)?;
        self.log_path = Some(path);
        Ok(())
    }

    pub fn log(&self, output: &str) -> Result<()> {
        if let Some(path) = &self.log_path {
            writeln!(fs::OpenOptions::new().append(true).open(path)?, "{output}")?;
        }
        Ok(())
    }

    pub fn fail(&mut self, err: &anyhow::Error) {
        self.status = "failed".into();
        let message = format!("{err:#}");
        let kind = if self.exit_code == 255
            || self.exit_code == 137
            || message.contains("worker unreachable")
            || message.contains("Broken pipe")
        {
            "infrastructure"
        } else {
            match self.stage.as_str() {
                "config" | "arguments" => "config",
                "queue" | "worker" | "fork" | "preflight" | "bootstrap" | "stop" | "sync"
                | "pull" | "cache" | "hydrate" => "infrastructure",
                "build"
                    if self.diagnostics.iter().any(|d| {
                        d.contains(": error:")
                            || d.starts_with("error[E")
                            || d.contains("undefined symbol")
                    }) =>
                {
                    "code"
                }
                _ => "unknown",
            }
        };
        self.failure_kind = Some(kind.into());
        self.retryable = kind == "infrastructure";
        if self.exit_code == 0 {
            self.exit_code = 1;
        }
        let _ = self.log(&message);
        if self.diagnostics.is_empty() {
            self.diagnostics.push(message);
        }
    }
}

pub fn load_jobs() -> Result<JobStore> {
    let dir = state_dir()?;
    let mut store: JobStore = match fs::read(dir.join("jobs.json")) {
        Ok(data) => serde_json::from_slice(&data)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => JobStore::default(),
        Err(e) => return Err(e.into()),
    };
    let jobs = dir.join("jobs");
    if jobs.is_dir() {
        for entry in fs::read_dir(jobs)? {
            let p = entry?.path();
            if p.extension().is_some_and(|e| e == "json") {
                store.jobs.push(serde_json::from_slice(&fs::read(p)?)?);
            }
        }
    }
    store.jobs.sort_by_key(|j| j.finished_at);
    Ok(store)
}

pub fn record_job(job: &JobRecord) -> Result<()> {
    atomic_write(
        &state_dir()?.join("jobs").join(format!("{}.json", job.id)),
        &serde_json::to_vec_pretty(job)?,
    )
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BuildRecord {
    pub id: String,
    pub job_id: String,
    pub project: String,
    pub name: String,
    pub notes: Option<String>,
    pub worktree_id: String,
    pub worktree_path: String,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub dirty: bool,
    pub snapshot_digest: String,
    pub sandbox_id: Option<String>,
    pub config: String,
    pub bundle_id: Option<String>,
    pub display_version: Option<String>,
    pub build_version: Option<String>,
    pub signing_kind: Option<String>,
    pub artifact_path: Option<String>,
    pub ipa_path: Option<String>,
    pub provider: Option<String>,
    pub firebase_app_id: Option<String>,
    pub testing_uri: Option<String>,
    pub firebase_console_uri: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl BuildRecord {
    pub fn new_id() -> String {
        let now = Utc::now().format("%Y%m%d-%H%M%S");
        let short = &uuid::Uuid::new_v4().simple().to_string()[..6];
        format!("bld-{now}-{short}")
    }
}

pub fn record_build(build: &BuildRecord) -> Result<()> {
    record_build_in(&state_dir()?.join("builds"), build)
}

pub fn load_builds() -> Result<Vec<BuildRecord>> {
    load_builds_from(&state_dir()?.join("builds"))
}

fn record_build_in(dir: &Path, build: &BuildRecord) -> Result<()> {
    atomic_write(
        &dir.join(format!("{}.json", build.id)),
        &serde_json::to_vec_pretty(build)?,
    )
}

fn load_builds_from(dir: &Path) -> Result<Vec<BuildRecord>> {
    let mut builds: Vec<BuildRecord> = Vec::new();
    if dir.is_dir() {
        for entry in fs::read_dir(dir)? {
            let p = entry?.path();
            if p.extension().is_some_and(|e| e == "json") {
                builds.push(serde_json::from_slice(&fs::read(p)?)?);
            }
        }
    }
    builds.sort_by_key(|a| std::cmp::Reverse(a.created_at));
    Ok(builds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn build_records_roundtrip_newest_first() {
        let dir = tempdir().unwrap();
        let mut older = BuildRecord {
            id: "bld-1".into(),
            project: "Demo".into(),
            name: "First".into(),
            created_at: Utc::now() - chrono::Duration::seconds(60),
            ..Default::default()
        };
        let newer = BuildRecord {
            id: "bld-2".into(),
            project: "Demo".into(),
            name: "Second".into(),
            testing_uri: Some("https://appdistribution.firebase.google.com/testerapps/x".into()),
            created_at: Utc::now(),
            ..Default::default()
        };
        older.branch = Some("main".into());
        record_build_in(dir.path(), &older).unwrap();
        record_build_in(dir.path(), &newer).unwrap();

        let loaded = load_builds_from(dir.path()).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "bld-2");
        assert_eq!(loaded[1].id, "bld-1");
        assert_eq!(
            loaded[0].testing_uri.as_deref(),
            Some("https://appdistribution.firebase.google.com/testerapps/x")
        );
    }

    #[test]
    fn errors_classify_transport_before_compiler_and_keep_unknown() {
        for (stage, code, diags, expected) in [
            (
                "build",
                255,
                vec!["x.swift:2: error: oops"],
                "infrastructure",
            ),
            ("build", 1, vec!["x.swift:2: error: oops"], "code"),
            ("build", 1, vec![], "unknown"),
            ("config", 1, vec![], "config"),
        ] {
            let mut job = JobRecord::new("build");
            job.stage = stage.into();
            job.exit_code = code;
            job.diagnostics = diags.into_iter().map(String::from).collect();
            job.fail(&anyhow::anyhow!("failure"));
            assert_eq!(job.failure_kind.as_deref(), Some(expected));
            assert!(!job.diagnostics.is_empty());
            assert_eq!(job.retryable, expected == "infrastructure");
        }
    }
}
