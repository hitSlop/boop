use crate::{
    cli::{BuildConfig, BuildTarget, Command, PoolCommand},
    config::cache_dir,
    provider::{WorkerEndpoint, ssh::control_path_args},
    state::atomic_write,
};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Seek},
    process::{Command as Process, Stdio},
    thread,
    time::{Duration, Instant},
};

pub struct Probe {
    pub arch: String,
    pub missing: bool,
    pub warm: bool,
    pub dependencies: bool,
    pub has_build_stamps: bool,
}

pub fn probe(
    ep: &WorkerEndpoint,
    ws: &str,
    engine: &str,
    command: &Command,
    deadline: Duration,
) -> Result<Probe> {
    let (target, config) = match command {
        Command::Build(a) | Command::Pool(PoolCommand::Warm(a)) => (a.target, a.config),
        Command::Publish(a) => (BuildTarget::Device, a.config),
        Command::Run(a) => (
            if a.simulator.is_some() {
                BuildTarget::Simulator
            } else {
                BuildTarget::Device
            },
            a.config,
        ),
        _ => (BuildTarget::Device, BuildConfig::Debug),
    };
    let configuration = match config {
        BuildConfig::Debug => "Debug",
        BuildConfig::Release => "Release",
    };
    let platform = match target {
        BuildTarget::Device => "iphoneos",
        BuildTarget::Simulator => "iphonesimulator",
    };
    let warm = if engine == "cargo" {
        format!("test -d '{ws}/target/{}' ", config.as_str())
    } else {
        format!(
            "test -f '{ws}/xtool/.xtool-tmp/DerivedData/Intermediates.noindex/XCBuildData/build.db' && test -d '{ws}/xtool/.xtool-tmp/DerivedData/Products/{configuration}-{platform}'"
        )
    };
    let stamp_check = if engine == "xtool" && ws.starts_with("/home/user/boop-projects/") {
        format!(
            "test -f \"$HOME/boop-cache/build-stamps/{}.json.gz\"",
            ws.rsplit('/').next().unwrap_or_default()
        )
    } else {
        "false".into()
    };
    run(
        ep,
        &warm,
        engine == "xtool",
        ws.starts_with("/home/user/boop-projects/"),
        deadline,
        &stamp_check,
    )
}

pub fn check_tools(ep: &WorkerEndpoint) -> Result<Probe> {
    run(ep, "false", true, false, WORKER_DEADLINE, "false")
}

pub const WORKER_DEADLINE: Duration = Duration::from_secs(5);
// Boat can report ready while sshd and the snapshot filesystem are still starting.
pub const FRESH_FORK_DEADLINE: Duration = Duration::from_secs(20);

fn run(
    ep: &WorkerEndpoint,
    warm: &str,
    check_tools: bool,
    pooled: bool,
    deadline: Duration,
    stamp_check: &str,
) -> Result<Probe> {
    let key = hex::encode(Sha256::digest(format!("{}\0{}", ep.provider, ep.id)));
    let cache = cache_dir()?.join("architectures").join(key);
    let cached = fs::read_to_string(&cache)
        .ok()
        .filter(|s| matches!(s.as_str(), "aarch64" | "arm64" | "x86_64"));
    let arch = cached
        .as_deref()
        .map(|v| format!("echo {v}"))
        .unwrap_or_else(|| "uname -m".into());
    let tools = if check_tools {
        r#"test -x "$HOME/.local/bin/xtool2" && test -x "$HOME/.local/bin/actool" && test -x "$HOME/.config/xtool/darwin.xtoolsdk/toolset/bin/bitcode_strip" "#
    } else {
        "true"
    };
    let cache_dir = if pooled {
        "$HOME/boop-cache"
    } else {
        "$HOME/.cache"
    };
    let script = format!(
        r#"set -e
{arch}
if {tools}; then echo ready; else echo missing; fi
if {warm}; then echo warm; else echo cold; fi
if test -n "$(find "{cache_dir}/org.swift.swiftpm/repositories" -mindepth 1 -maxdepth 1 -print -quit 2>/dev/null)"; then echo cached; else echo empty; fi
if {stamp_check}; then echo stamps; else echo none; fi"#
    );
    let output = bounded_ssh(ep, &script, deadline)?;
    let lines: Vec<_> = output.lines().collect();
    ensure!(
        lines.len() == 5
            && matches!(lines[1], "ready" | "missing")
            && matches!(lines[2], "warm" | "cold")
            && matches!(lines[3], "cached" | "empty")
            && matches!(lines[4], "stamps" | "none"),
        "invalid worker preflight response: {output}"
    );
    let arch = lines[0].to_owned();
    if cached.as_ref() != Some(&arch) {
        atomic_write(&cache, arch.as_bytes())?;
    }
    Ok(Probe {
        arch,
        missing: lines[1] == "missing",
        warm: lines[2] == "warm",
        dependencies: lines[3] == "cached",
        has_build_stamps: lines[4] == "stamps",
    })
}

fn bounded_ssh(ep: &WorkerEndpoint, script: &str, deadline: Duration) -> Result<String> {
    let start = Instant::now();
    let mut out = tempfile::tempfile()?;
    let mut err = tempfile::tempfile()?;
    let mut child = Process::new("ssh")
        .args([
            "-o",
            &format!("ConnectTimeout={}", deadline.as_secs()),
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPersist=no",
        ])
        .args(control_path_args(ep))
        .arg(format!("{}@{}", ep.user, ep.host))
        .arg(script)
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(err.try_clone()?)
        .spawn()
        .context("worker unreachable during SSH handshake")?;
    loop {
        if let Some(status) = child.try_wait()? {
            let mut msg = String::new();
            if !status.success() {
                err.rewind()?;
                err.read_to_string(&mut msg)?;
                if status.code() == Some(255) {
                    bail!("worker unreachable during SSH handshake: {}", msg.trim());
                }
                bail!("worker preflight exited {status}: {}", msg.trim());
            }
            out.rewind()?;
            out.read_to_string(&mut msg)?;
            return Ok(msg);
        }
        if start.elapsed() >= deadline {
            let _ = child.kill();
            child.wait()?;
            bail!(
                "worker unreachable during SSH handshake: {}-second preflight deadline exceeded",
                deadline.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(15));
    }
}
