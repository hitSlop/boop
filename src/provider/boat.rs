use crate::config::home_dir;
use crate::provider::ssh::{exec_ssh_stream, rsync_pull, rsync_push};
use crate::provider::{ExecResult, SyncStats, WorkerEndpoint, WorkerProvider, WorkerStatus};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone)]
pub struct BoatProvider {
    pub sandbox_id: String,
    pub hourly_rate_usd: f64,
    pub ssh_key: Option<String>,
}

impl BoatProvider {
    pub fn new(sandbox_id: String, hourly_rate_usd: f64, ssh_key: Option<String>) -> Self {
        Self {
            sandbox_id,
            hourly_rate_usd,
            ssh_key,
        }
    }

    pub fn boat_bin() -> PathBuf {
        if let Ok(home) = home_dir() {
            let local = home.join(".local/bin/boat");
            if local.exists() {
                return local;
            }
        }
        PathBuf::from("boat")
    }

    fn default_ssh_key(&self) -> Result<String> {
        if let Some(ref k) = self.ssh_key {
            return Ok(k.clone());
        }
        Ok(home_dir()?
            .join(".ssh/ascii_box_ed25519")
            .to_string_lossy()
            .to_string())
    }

    fn fetch_info(&self) -> Result<Value> {
        let out = Command::new(Self::boat_bin())
            .args(["info", &self.sandbox_id, "--json", "--no-update"])
            .output()
            .context("Failed to execute `boat info`")?;
        if !out.status.success() {
            bail!(
                "`boat info {}` failed: {}",
                self.sandbox_id,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let val: Value = serde_json::from_slice(&out.stdout)
            .context("Failed to parse JSON from `boat info --json`")?;
        Ok(val)
    }

    pub fn state(&self) -> Result<String> {
        self.fetch_info()?["sandbox"]["state"]
            .as_str()
            .map(String::from)
            .context("Boat response missing sandbox.state")
    }

    /// Wait for a restored disk to leave Boat's temporary, second-resolution filesystem.
    pub fn wait_for_disk(&self) -> Result<()> {
        let started = std::time::Instant::now();
        let mut announced = false;
        loop {
            let info = self.fetch_info()?;
            let sandbox = &info["sandbox"];
            anyhow::ensure!(
                matches!(
                    sandbox["state"].as_str(),
                    Some("ready" | "idle" | "running")
                ),
                "Boat sandbox {} stopped while restoring its disk: {}",
                self.sandbox_id,
                sandbox["state"]
            );
            if sandbox["hydrated"].as_bool() != Some(false) {
                return Ok(());
            }
            if !announced {
                eprintln!(
                    "Waiting for Boat to restore the build cache on {}...",
                    self.sandbox_id
                );
                announced = true;
            }
            anyhow::ensure!(
                started.elapsed() < std::time::Duration::from_secs(900),
                "Boat disk restoration timed out for {}: {}",
                self.sandbox_id,
                sandbox["hydration"]
            );
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }

    pub fn fork_id(raw: &str) -> Result<String> {
        let value: Value = serde_json::from_str(raw)?;
        value["id"]
            .as_str()
            .or_else(|| value["sandbox"]["id"].as_str())
            .map(String::from)
            .context("Boat fork response missing sandbox ID")
    }

    pub fn fork(&self, path: &Path, receipt: std::fs::File) -> Result<()> {
        let out = Command::new(Self::boat_bin())
            .args(["fork", &self.sandbox_id, "--json", "--no-update"])
            .stdout(receipt)
            .stderr(std::process::Stdio::piped())
            .spawn()?
            .wait_with_output()?;
        if !out.status.success() {
            let raw = std::fs::read_to_string(path)?;
            if Self::fork_id(&raw).is_err() {
                std::fs::remove_file(path)?;
            }
            bail!(
                "boat fork {}: {}{}",
                self.sandbox_id,
                raw,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }

    pub fn delete(&self) -> Result<()> {
        let out = Command::new(Self::boat_bin())
            .args(["delete", &self.sandbox_id, "--yes", "--json", "--no-update"])
            .output()?;
        if !out.status.success() {
            let msg = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            if !msg.contains("sandbox_not_found") && !msg.contains("404") {
                bail!("boat delete {}: {msg}", self.sandbox_id);
            }
        }
        Ok(())
    }

    pub fn create_sandbox(name: &str, machine_type: &str) -> Result<String> {
        let out = Command::new(Self::boat_bin())
            .args([
                "new",
                "--type",
                machine_type,
                "--name",
                name,
                "--no-env",
                "--json",
                "--no-update",
            ])
            .output()
            .context("Failed to execute `boat new`")?;
        if !out.status.success() {
            bail!(
                "`boat new` failed: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines().rev() {
            if let Ok(v) = serde_json::from_str::<Value>(line)
                && let Some(id) = v.get("id").and_then(|x| x.as_str())
            {
                return Ok(id.to_string());
            }
        }
        bail!("Could not parse sandbox id from `boat new --json` output:\n{stdout}")
    }
}

fn parse_host_port(endpoint: &str) -> Result<(String, u16)> {
    let (host, port_str) = endpoint
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("Invalid sshEndpoint '{endpoint}'"))?;
    let port: u16 = port_str
        .parse()
        .with_context(|| format!("Invalid port in sshEndpoint '{endpoint}'"))?;
    Ok((host.to_string(), port))
}

fn format_boat_usage(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        let dollars = v["dollars"].as_f64().unwrap_or(0.0);
        let secs = v["seconds"].as_u64().unwrap_or(0);
        let mins = secs / 60;
        let rem = secs % 60;
        let running = v["running"].as_bool().unwrap_or(false);
        return format!(
            "Usage: ${dollars:.4} ({mins}m {rem}s, {})",
            if running {
                "billing active"
            } else {
                "paused — $0/hr"
            }
        );
    }
    trimmed.to_string()
}

impl WorkerProvider for BoatProvider {
    fn provider_name(&self) -> &'static str {
        "boat"
    }

    fn ensure_running(&self) -> Result<WorkerEndpoint> {
        let mut info = self.fetch_info()?;
        let state = info["sandbox"]["state"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();

        if matches!(state.as_str(), "stopped" | "paused" | "archived") {
            eprintln!("Resuming Boat sandbox {}...", self.sandbox_id);
            let out = Command::new(Self::boat_bin())
                .args(["resume", &self.sandbox_id, "--json", "--no-update"])
                .output()?;
            if !out.status.success() {
                bail!(
                    "boat resume {}: {}{}",
                    self.sandbox_id,
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            info = self.fetch_info()?;
        }
        let start = std::time::Instant::now();
        loop {
            match info["sandbox"]["state"].as_str().unwrap_or("unknown") {
                "running" | "ready" | "idle" => break,
                "starting" | "provisioning" | "provisioned" | "cloning"
                    if start.elapsed().as_secs() < 180 =>
                {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    info = self.fetch_info()?;
                }
                other => bail!(
                    "Boat sandbox {} in state {other}: {}",
                    self.sandbox_id,
                    info["sandbox"]["error"]
                ),
            }
        }

        let ssh_ep = info["sandbox"]["sshEndpoint"].as_str().ok_or_else(|| {
            anyhow::anyhow!("Boat sandbox {} has no sshEndpoint yet", self.sandbox_id)
        })?;
        let (host, port) = parse_host_port(ssh_ep)?;
        let key_path = self.default_ssh_key()?;

        // Ensure Boat has pushed our local SSH key into the sandbox if key isn't present yet
        if !Path::new(&key_path).exists() {
            let _ = Command::new(Self::boat_bin())
                .args(["ssh", &self.sandbox_id, "--no-update", "true"])
                .output();
        }

        Ok(WorkerEndpoint {
            provider: "boat".into(),
            hydrating: info["sandbox"]["hydrated"].as_bool() == Some(false),
            id: self.sandbox_id.clone(),
            host,
            port,
            user: "user".into(),
            ssh_key: Some(key_path),
            hourly_rate_usd: self.hourly_rate_usd,
        })
    }

    fn stop(&self) -> Result<()> {
        if matches!(self.state()?.as_str(), "stopped" | "archived") {
            return Ok(());
        }
        let out = Command::new(Self::boat_bin())
            .args(["stop", &self.sandbox_id, "--json", "--no-update"])
            .output()?;
        if !out.status.success() {
            bail!(
                "boat stop {}: {}{}",
                self.sandbox_id,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let start = std::time::Instant::now();
        loop {
            let info = self.fetch_info()?;
            let sb = &info["sandbox"];
            if matches!(sb["state"].as_str(), Some("stopped" | "archived")) {
                break;
            }
            if sb["stop"]["status"] == "failing" || start.elapsed().as_secs() >= 300 {
                bail!(
                    "waiting for Boat stop {}: state={} stop={}",
                    self.sandbox_id,
                    sb["state"],
                    sb["stop"]
                );
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        Ok(())
    }

    fn status(&self) -> Result<WorkerStatus> {
        let info = self.fetch_info()?;
        let sb = &info["sandbox"];
        let usage = Command::new(Self::boat_bin())
            .args(["usage", &self.sandbox_id, "--no-update"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| format_boat_usage(&String::from_utf8_lossy(&o.stdout)));

        Ok(WorkerStatus {
            provider: "boat".into(),
            id: self.sandbox_id.clone(),
            state: sb["state"].as_str().unwrap_or("unknown").to_string(),
            vcpu: sb["vcpu"].as_u64().map(|v| v as u32),
            memory_gb: sb["memoryGB"].as_u64().map(|v| v as u32),
            ssh_endpoint: sb["sshEndpoint"].as_str().map(|s| s.to_string()),
            usage_summary: usage,
        })
    }

    fn sync_push(
        &self,
        endpoint: &WorkerEndpoint,
        local_dir: &Path,
        remote_dir: &str,
        excludes: &[String],
    ) -> Result<SyncStats> {
        rsync_push(endpoint, local_dir, remote_dir, excludes)
    }

    fn sync_pull(
        &self,
        endpoint: &WorkerEndpoint,
        remote_path: &str,
        local_dir: &Path,
    ) -> Result<u64> {
        rsync_pull(endpoint, remote_path, local_dir)
    }

    fn exec_stream(
        &self,
        endpoint: &WorkerEndpoint,
        remote_cmd: &str,
        stream_stdout: bool,
    ) -> Result<ExecResult> {
        exec_ssh_stream(endpoint, remote_cmd, stream_stdout)
    }
}
