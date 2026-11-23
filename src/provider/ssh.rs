use crate::config::cache_dir;
use crate::provider::{ExecResult, SyncStats, WorkerEndpoint, WorkerProvider, WorkerStatus};
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

pub struct SshProvider {
    pub host: String,
    pub user: String,
    pub port: u16,
    pub ssh_key: Option<String>,
    pub hourly_rate_usd: f64,
}

impl SshProvider {
    pub fn new(
        host: String,
        user: String,
        port: u16,
        ssh_key: Option<String>,
        hourly_rate_usd: f64,
    ) -> Self {
        Self {
            host,
            user,
            port,
            ssh_key,
            hourly_rate_usd,
        }
    }
}

impl WorkerProvider for SshProvider {
    fn provider_name(&self) -> &'static str {
        "ssh"
    }

    fn ensure_running(&self) -> Result<WorkerEndpoint> {
        Ok(WorkerEndpoint {
            provider: "ssh".into(),
            hydrating: false,
            id: format!("{}@{}:{}", self.user, self.host, self.port),
            host: self.host.clone(),
            port: self.port,
            user: self.user.clone(),
            ssh_key: self.ssh_key.clone(),
            hourly_rate_usd: self.hourly_rate_usd,
        })
    }

    fn stop(&self) -> Result<()> {
        eprintln!(
            "Generic SSH worker {}@{} does not support remote stop/pause.",
            self.user, self.host
        );
        Ok(())
    }

    fn status(&self) -> Result<WorkerStatus> {
        let ep = self.ensure_running()?;
        let ok = exec_ssh_stream(&ep, "echo ok", false)
            .map(|r| r.exit_code == 0)
            .unwrap_or(false);
        Ok(WorkerStatus {
            provider: "ssh".into(),
            id: ep.id,
            state: if ok { "reachable" } else { "unreachable" }.into(),
            vcpu: None,
            memory_gb: None,
            ssh_endpoint: Some(format!("{}:{}", self.host, self.port)),
            usage_summary: None,
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

pub(crate) fn control_path_args(ep: &WorkerEndpoint) -> Vec<String> {
    let mut args = vec![
        "-p".into(),
        ep.port.to_string(),
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "LogLevel=ERROR".into(),
        "-o".into(),
        "ConnectTimeout=15".into(),
        "-o".into(),
        "ServerAliveInterval=30".into(),
        "-o".into(),
        "ServerAliveCountMax=2".into(),
        "-o".into(),
        "BatchMode=yes".into(),
    ];
    if let Ok(cdir) = cache_dir() {
        let ctl_dir = cdir.join("ssh-ctl");
        if fs::create_dir_all(&ctl_dir).is_ok() {
            // Keep Unix socket path short for macOS 104-char sun_path limit
            let ctl = "/tmp/boop-ssh-%C";
            args.push("-o".into());
            args.push("ControlMaster=auto".into());
            args.push("-o".into());
            args.push(format!("ControlPath={ctl}"));
            args.push("-o".into());
            args.push("ControlPersist=5m".into());
        }
    }
    if let Some(ref key) = ep.ssh_key {
        args.push("-i".into());
        args.push(key.clone());
    }
    args
}

fn ssh_shell_string(ep: &WorkerEndpoint) -> String {
    let mut parts = vec!["ssh".to_string()];
    for a in control_path_args(ep) {
        if a.contains(' ') {
            parts.push(format!("'{a}'"));
        } else {
            parts.push(a);
        }
    }
    parts.join(" ")
}

pub fn exec_ssh_stream(
    ep: &WorkerEndpoint,
    remote_cmd: &str,
    stream_stdout: bool,
) -> Result<ExecResult> {
    let start = Instant::now();
    let mut cmd = Command::new("ssh");
    cmd.args(control_path_args(ep));
    cmd.arg(format!("{}@{}", ep.user, ep.host));
    cmd.arg(remote_cmd);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn ssh to {}@{}:{}", ep.user, ep.host, ep.port))?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let out_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if stream_stdout {
                println!("{line}");
            }
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });

    let err_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            if stream_stdout {
                eprintln!("{line}");
            }
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });

    let status = child.wait()?;
    let out = out_handle.join().unwrap_or_default();
    let err = err_handle.join().unwrap_or_default();
    let mut combined = out;
    if !err.is_empty() {
        combined.push_str(&err);
    }

    Ok(ExecResult {
        exit_code: status.code().unwrap_or(1),
        output: combined,
        duration_ms: start.elapsed().as_millis() as u64,
    })
}

pub fn rsync_push(
    ep: &WorkerEndpoint,
    local_dir: &Path,
    remote_dir: &str,
    excludes: &[String],
) -> Result<SyncStats> {
    let start = Instant::now();
    // Ensure remote directory exists first
    let mkdir_res = exec_ssh_stream(ep, &format!("mkdir -p '{remote_dir}'"), false)?;
    if mkdir_res.exit_code != 0 {
        bail!(
            "Failed to create remote directory {remote_dir}: {}",
            mkdir_res.output
        );
    }

    let mut src = local_dir.to_string_lossy().to_string();
    if !src.ends_with('/') {
        src.push('/');
    }
    let dst = format!(
        "{}@{}:{}/",
        ep.user,
        ep.host,
        remote_dir.trim_end_matches('/')
    );

    let mut cmd = Command::new("rsync");
    cmd.args(["-az", "--delete", "--stats", "-e", &ssh_shell_string(ep)]);
    if remote_dir.starts_with("/home/user/boop-projects/") {
        // Git checkouts have fresh mtimes even when every source byte matches the base.
        cmd.args(["--checksum", "--no-times"]);
    }
    for ex in excludes {
        cmd.arg(format!("--exclude={ex}"));
    }
    cmd.arg(&src);
    cmd.arg(&dst);

    let output = cmd.output().context("Failed to execute rsync push")?;
    if !output.status.success() {
        bail!(
            "rsync push failed (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let (files, bytes) = parse_rsync_stats(&stdout);
    Ok(SyncStats {
        files_transferred: files,
        bytes_sent: bytes,
        duration_ms: start.elapsed().as_millis() as u64,
    })
}

pub fn rsync_pull(ep: &WorkerEndpoint, remote_path: &str, local_dir: &Path) -> Result<u64> {
    let start = Instant::now();
    fs::create_dir_all(local_dir)?;
    let src = format!(
        "{}@{}:{}/",
        ep.user,
        ep.host,
        remote_path.trim_end_matches('/')
    );
    let mut dst = local_dir.to_string_lossy().to_string();
    if !dst.ends_with('/') {
        dst.push('/');
    }

    let output = Command::new("rsync")
        .args([
            "-a",
            "--compress",
            "--compress-level=1",
            "--no-whole-file",
            "--inplace",
            "--delete",
            "-e",
            &ssh_shell_string(ep),
            &src,
            &dst,
        ])
        .output()
        .context("Failed to execute rsync pull")?;

    if !output.status.success() {
        bail!(
            "rsync pull failed (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(start.elapsed().as_millis() as u64)
}

fn parse_rsync_stats(stdout: &str) -> (usize, u64) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    for line in stdout.lines() {
        let t = line.trim();
        if let Some(rest) = t
            .strip_prefix("Number of regular files transferred:")
            .or_else(|| t.strip_prefix("Number of files transferred:"))
        {
            if let Some(num) = rest.split_whitespace().next() {
                files = num.replace(',', "").parse().unwrap_or(0);
            }
        } else if let Some(rest) = t
            .strip_prefix("Total sent:")
            .or_else(|| t.strip_prefix("Total bytes sent:"))
            && let Some(num) = rest.split_whitespace().next()
        {
            bytes = num.replace(',', "").parse().unwrap_or(0);
        }
    }
    (files, bytes)
}
