pub mod boat;
pub mod ssh;

use crate::config::WorkerConfig;
use anyhow::Result;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct WorkerEndpoint {
    pub provider: String,
    pub hydrating: bool,
    pub id: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub ssh_key: Option<String>,
    pub hourly_rate_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerStatus {
    pub provider: String,
    pub id: String,
    pub state: String,
    pub vcpu: Option<u32>,
    pub memory_gb: Option<u32>,
    pub ssh_endpoint: Option<String>,
    pub usage_summary: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncStats {
    pub files_transferred: usize,
    pub bytes_sent: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ExecResult {
    pub exit_code: i32,
    pub output: String,
    pub duration_ms: u64,
}

pub trait WorkerProvider {
    fn provider_name(&self) -> &'static str;
    fn ensure_running(&self) -> Result<WorkerEndpoint>;
    fn stop(&self) -> Result<()>;
    fn status(&self) -> Result<WorkerStatus>;
    fn sync_push(
        &self,
        endpoint: &WorkerEndpoint,
        local_dir: &Path,
        remote_dir: &str,
        excludes: &[String],
    ) -> Result<SyncStats>;
    fn sync_pull(
        &self,
        endpoint: &WorkerEndpoint,
        remote_path: &str,
        local_dir: &Path,
    ) -> Result<u64>;
    fn exec_stream(
        &self,
        endpoint: &WorkerEndpoint,
        remote_cmd: &str,
        stream_stdout: bool,
    ) -> Result<ExecResult>;
}

pub fn create_provider(cfg: &WorkerConfig) -> Box<dyn WorkerProvider> {
    match cfg {
        WorkerConfig::Boat {
            sandbox_id,
            hourly_rate_usd,
            ssh_key,
            ..
        } => Box::new(boat::BoatProvider::new(
            sandbox_id.clone(),
            *hourly_rate_usd,
            ssh_key.clone(),
        )),
        WorkerConfig::Ssh {
            host,
            user,
            port,
            ssh_key,
            hourly_rate_usd,
        } => Box::new(ssh::SshProvider::new(
            host.clone(),
            user.clone(),
            *port,
            ssh_key.clone(),
            *hourly_rate_usd,
        )),
    }
}
