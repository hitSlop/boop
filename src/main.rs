mod apple;
mod cli;
mod config;
mod distribute;
mod engine;
mod pool;
mod preflight;
mod provider;
mod snapshot;
mod state;

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use clap::Parser;
use cli::PoolCommand;
use cli::{
    BuildArgs, BuildTarget, BuildsArgs, Cli, Command, ExecArgs, InitArgs, ProviderKind,
    PublishArgs, PublishSigningProfile, RunArgs,
};
use config::{
    WorkerConfig, boat_hourly_rate, detect_firebase_app_id, detect_project, find_project_root,
    load_global_config, load_project_config, resolve_worker, save_global_config,
    save_project_config,
};
use pool::Pool;
use provider::boat::BoatProvider;
use provider::{WorkerEndpoint, WorkerProvider, create_provider};
use serde_json::json;
use sha2::Digest;
use snapshot::compute_snapshot;
use state::{BuildRecord, JobRecord, load_builds, load_jobs, record_build, record_job};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn main() {
    let started = Instant::now();
    let cli = Cli::try_parse();
    let json = cli.as_ref().map(|c| c.json).unwrap_or_else(|_| {
        std::env::args_os()
            .skip(1)
            .take_while(|arg| arg != "--")
            .any(|arg| arg == "--json")
    });
    let mut job = JobRecord::new(cli.as_ref().map_or("boop", |c| match c.command {
        Command::Init(_) => "init",
        Command::Run(_) => "run",
        Command::Build(_) => "build",
        Command::Publish(_) => "publish",
        Command::Builds(_) => "builds",
        Command::Exec(_) => "exec",
        Command::Status => "status",
        Command::Stop(_) => "stop",
        Command::Doctor => "doctor",
        Command::Release => "release",
        Command::Pool(PoolCommand::Warm(_)) => "pool warm",
        Command::Pool(PoolCommand::Status) => "pool status",
        Command::Pool(PoolCommand::Gc) => "pool gc",
    }));
    let result = job.open_log().and_then(|()| match cli {
        Ok(cli) => run_cli(cli, &mut job),
        Err(e) if !json => e.exit(),
        Err(e) if e.exit_code() == 0 => {
            job.details.insert("help".into(), json!(e.to_string()));
            Ok(())
        }
        Err(e) => {
            job.stage = "arguments".into();
            job.exit_code = e.exit_code();
            Err(e.into())
        }
    });
    job.finished_at = Utc::now();
    job.total_ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(()) => {
            job.status = "completed".into();
            job.stage = "complete".into();
        }
        Err(e) => {
            job.fail(&e);
            eprintln!("{e:#}");
        }
    }
    if let Err(e) = record_job(&job) {
        eprintln!("saving job result: {e:#}");
        if job.exit_code == 0 {
            job.stage = "record".into();
            job.fail(&e);
        }
    }
    if json {
        println!(
            "{}",
            serde_json::to_string(&job).expect("job fields serialize as JSON")
        );
    } else if job.status == "completed"
        && matches!(
            job.command.as_str(),
            "build" | "run" | "publish" | "pool warm"
        )
    {
        println!(
            "{} completed in {:.2}s",
            job.command,
            job.total_ms as f64 / 1000.
        );
        if let Some(path) = &job.artifact_path {
            println!("Artifact: {path}");
        }
        if let Some(id) = job.details.get("build_id").and_then(|v| v.as_str()) {
            println!("Build ID: {id}");
        }
        if let Some(url) = job.details.get("testing_uri").and_then(|v| v.as_str()) {
            println!("Testing URL: {url}");
        }
    }
    std::process::exit(job.exit_code.clamp(0, 255))
}

fn run_cli(cli: Cli, job: &mut JobRecord) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let worker = cli.worker.as_deref();
    match cli.command {
        Command::Init(args) => cmd_init(&cwd, args, cli.json, job),
        Command::Doctor => cmd_doctor(&cwd, worker, cli.json, job),
        Command::Status => cmd_status(&cwd, worker, cli.json, job),
        Command::Builds(args) => cmd_builds(args, cli.json, job),
        Command::Publish(args) if args.no_build => {
            let root = find_project_root(&cwd).unwrap_or(cwd).canonicalize()?;
            let project = load_project_config(&root)?;
            job.project = project.project.name.clone();
            job.worktree_id = snapshot::compute_worktree_id(&root)?;
            let app_name = project
                .project
                .app_name
                .as_deref()
                .or(project.project.scheme.as_deref())
                .unwrap_or(&project.project.name);
            let app = root.join(".boop/artifacts").join(format!("{app_name}.app"));
            ensure!(
                app.is_dir(),
                "No pulled app found at {}. Run `boop build` or `boop publish` (without --no-build) first.",
                app.display()
            );
            job.artifact_path = Some(app.display().to_string());
            publish_app_bundle(&args, &project, &root, &app, !cli.json, job)
        }
        command => {
            let root = find_project_root(&cwd).unwrap_or(cwd).canonicalize()?;
            let project = if root.join(config::PROJECT_CONFIG_FILE).exists() {
                Some(load_project_config(&root)?)
            } else {
                None
            };
            let global = load_global_config()?;
            let override_name = if let Command::Stop(a) = &command {
                a.name.as_deref().or(worker)
            } else {
                worker
            };
            let (name, cfg) = resolve_worker(
                &global,
                override_name,
                project.as_ref().and_then(|p| p.project.worker.as_deref()),
            )?;
            job.worker = name.clone();
            job.worktree_id = snapshot::compute_worktree_id(&root)?;
            if let Some(p) = &project {
                job.project = p.project.name.clone();
            }
            let pool = Pool::configured(&name, cfg, &global)?;
            let need_pool = || {
                pool.as_ref()
                    .context("configure [workers.<name>.pool] for this Boat worker")
            };
            match command {
                Command::Pool(PoolCommand::Status) => {
                    job.stage = "worker".into();
                    report(need_pool()?.status()?, cli.json, job)
                }
                Command::Pool(PoolCommand::Gc) | Command::Release => {
                    job.stage = "release".into();
                    let lease = matches!(command, Command::Release).then_some(root.as_path());
                    report(json!({"deleted":need_pool()?.gc(lease)?}), cli.json, job)
                }
                Command::Stop(_) => {
                    job.stage = "stop".into();
                    if let Some(pool) = pool {
                        report(json!({"stopped":pool.stop()?}), cli.json, job)
                    } else {
                        create_provider(cfg).stop()?;
                        report(json!({"stopped":[name]}), cli.json, job)
                    }
                }
                Command::Exec(_)
                | Command::Run(_)
                | Command::Build(_)
                | Command::Publish(_)
                | Command::Pool(PoolCommand::Warm(_)) => {
                    if !matches!(command, Command::Exec(_)) {
                        ensure!(
                            project.is_some(),
                            "No .boop.toml found. Run boop init first."
                        );
                    }
                    let warm = matches!(command, Command::Pool(PoolCommand::Warm(_)));
                    let ws = if pool.is_some() {
                        project_workspace(&root, project.as_ref())?
                    } else {
                        format!("/home/user/boop-ws/{}", job.worktree_id)
                    };
                    job.remote_workspace = Some(ws.clone());
                    let run_target = if let Command::Run(args) = &command {
                        Some(RunTarget::resolve(
                            args,
                            project.as_ref().context("project missing")?,
                        )?)
                    } else {
                        None
                    };
                    job.stage = "queue".into();
                    let assignment = if warm {
                        Some(need_pool()?.base_job(job, cli.no_wait)?)
                    } else {
                        pool.as_ref()
                            .map(|p| p.acquire(&root, cli.no_wait, job))
                            .transpose()?
                    };
                    let single = create_provider(cfg);
                    let provider: &dyn WorkerProvider = assignment
                        .as_ref()
                        .map(|a| &a.provider as &dyn WorkerProvider)
                        .unwrap_or(single.as_ref());
                    if let Some(a) = &assignment {
                        job.sandbox_id = Some(a.provider.sandbox_id.clone());
                    } else if let WorkerConfig::Boat { sandbox_id, .. } = cfg {
                        job.sandbox_id = Some(sandbox_id.clone());
                    }
                    job.stage = "worker".into();
                    let result = (|| {
                        let ep = provider.ensure_running()?;
                        let mut ctx = BuildContext {
                            root: &root,
                            provider,
                            ep: &ep,
                            ws: &ws,
                            stream: !cli.json,
                        };
                        let engine = project
                            .as_ref()
                            .map(|p| p.project.engine.as_str())
                            .unwrap_or("");
                        job.stage = "preflight".into();
                        let fork_start = assignment.as_ref().and_then(|a| a.created);
                        let deadline = if fork_start.is_some() {
                            preflight::FRESH_FORK_DEADLINE
                        } else {
                            preflight::WORKER_DEADLINE
                        };
                        let probe = preflight::probe(&ep, &ws, engine, &command, deadline)?;
                        if let Some(start) = fork_start {
                            job.fork_ms = Some(start.elapsed().as_millis() as u64);
                        }
                        job.build_dir_warm = Some(probe.warm);
                        job.dependency_cache_warm = Some(probe.dependencies);
                        if engine == "xtool"
                            && (probe.has_build_stamps || warm || (ep.hydrating && probe.warm))
                            && let Some(assigned) = &assignment
                        {
                            job.stage = "hydrate".into();
                            let started = Instant::now();
                            let result = assigned.provider.wait_for_disk();
                            job.details.insert(
                                "hydration_ms".into(),
                                json!(started.elapsed().as_millis()),
                            );
                            result?;
                        }
                        if probe.has_build_stamps {
                            ctx.build_stamps("restore", job)?;
                        }
                        if probe.missing && !matches!(command, Command::Exec(_)) {
                            job.stage = "bootstrap".into();
                            let start = Instant::now();
                            let result = apple::sdk::bootstrap_worker(provider, &ep, &probe.arch);
                            job.bootstrap_ms = start.elapsed().as_millis() as u64;
                            result?;
                        }
                        match command {
                            Command::Exec(args) => ctx.exec(args, project.as_ref(), job),
                            Command::Run(args) => {
                                let p = project.as_ref().context("project missing")?;
                                let target = run_target.context("device missing")?;
                                let build = BuildArgs {
                                    target: target.platform(),
                                    config: args.config,
                                    scheme: args.scheme.clone(),
                                    verbose: args.verbose,
                                    clean: false,
                                    name: None,
                                    notes: None,
                                };
                                let app = ctx
                                    .build(p, &build, job)?
                                    .context("build produced no app")?;
                                target.deploy(&args, p, &root, &app, job)
                            }
                            Command::Publish(args) => {
                                let p = project.as_ref().context("project missing")?;
                                let build = BuildArgs {
                                    target: BuildTarget::Device,
                                    config: args.config,
                                    scheme: args.scheme.clone(),
                                    verbose: args.verbose,
                                    clean: args.clean,
                                    name: args.name.clone(),
                                    notes: args.notes.clone(),
                                };
                                let app = ctx
                                    .build(p, &build, job)?
                                    .context("build produced no app")?;
                                publish_app_bundle(&args, p, &root, &app, !cli.json, job)
                            }
                            Command::Build(args) | Command::Pool(PoolCommand::Warm(args)) => {
                                let p = project.as_ref().context("project missing")?;
                                let app = ctx.build(p, &args, job)?;
                                if warm && engine == "xtool" {
                                    ctx.build_stamps("capture", job)?;
                                }
                                if !warm && (args.name.is_some() || args.notes.is_some()) {
                                    let (branch, commit, dirty) = read_git_meta(&root);
                                    let build_id = BuildRecord::new_id();
                                    let build_name = args
                                        .name
                                        .clone()
                                        .or_else(|| branch.clone())
                                        .unwrap_or_else(|| p.project.name.clone());
                                    let rec = BuildRecord {
                                        id: build_id.clone(),
                                        job_id: job.id.clone(),
                                        project: p.project.name.clone(),
                                        name: build_name,
                                        notes: args.notes.clone(),
                                        worktree_id: job.worktree_id.clone(),
                                        worktree_path: root.display().to_string(),
                                        branch,
                                        commit,
                                        dirty,
                                        snapshot_digest: job.snapshot_digest.clone(),
                                        sandbox_id: job.sandbox_id.clone(),
                                        config: args.config.as_str().into(),
                                        bundle_id: p.project.bundle_id.clone(),
                                        artifact_path: app.map(|a| a.display().to_string()),
                                        created_at: Utc::now(),
                                        ..Default::default()
                                    };
                                    record_build(&rec)?;
                                    job.details.insert("build_id".into(), json!(build_id));
                                }
                                Ok(())
                            }
                            _ => unreachable!(),
                        }
                    })();
                    if warm {
                        let stop = provider.stop();
                        if let Err(stop_err) = stop {
                            if let Err(e) = result {
                                bail!("{e:#}; stopping base: {stop_err:#}");
                            }
                            job.stage = "stop".into();
                            return Err(stop_err);
                        }
                    }
                    result
                }
                _ => unreachable!(),
            }
        }
    }
}

fn project_workspace(root: &Path, project: Option<&config::ProjectConfig>) -> Result<String> {
    let git = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
            "--show-prefix",
        ])
        .output()?;
    let identity = if git.status.success() {
        String::from_utf8(git.stdout)?
    } else {
        root.display().to_string()
    };
    let name = project.map(|p| p.project.name.as_str()).unwrap_or("");
    let hash = hex::encode(sha2::Sha256::digest(format!("{identity}\0{name}")));
    Ok(format!("/home/user/boop-projects/{}", &hash[..16]))
}

fn report(value: serde_json::Value, json_out: bool, job: &mut JobRecord) -> Result<()> {
    if !json_out {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    for (key, value) in value.as_object().context("expected a result object")? {
        job.details.insert(key.clone(), value.clone());
    }
    Ok(())
}

fn cmd_init(cwd: &Path, args: InitArgs, json_out: bool, job: &mut JobRecord) -> Result<()> {
    let mut project_cfg = detect_project(cwd)?;

    // Auto-detect preferred physical iPhone if available
    if project_cfg.project.engine == "xtool"
        && let Ok(dev) = apple::device::resolve_physical_device(None)
    {
        project_cfg.project.default_device = Some(dev.name);
    }
    project_cfg.project.worker = Some(args.worker_name.clone());
    job.project = project_cfg.project.name.clone();
    job.worker = args.worker_name.clone();

    let mut global_cfg = load_global_config()?;
    let worker_cfg = match args.provider {
        ProviderKind::Boat => {
            let sandbox_id = if let Some(id) = args.boat_id {
                id
            } else {
                let name = format!("boop-{}", project_cfg.project.name.to_lowercase());
                eprintln!(
                    "==> Provisioning new Boat sandbox '{}' (type={})...",
                    name, args.machine_type
                );
                job.stage = "worker".into();
                BoatProvider::create_sandbox(&name, &args.machine_type)?
            };
            job.sandbox_id = Some(sandbox_id.clone());
            let rate = boat_hourly_rate(&args.machine_type);
            let pool = match global_cfg.workers.get(&args.worker_name) {
                Some(WorkerConfig::Boat {
                    sandbox_id: existing,
                    pool,
                    ..
                }) if existing == &sandbox_id => pool.clone(),
                _ => None,
            };
            WorkerConfig::Boat {
                sandbox_id,
                pool,
                machine_type: args.machine_type.clone(),
                hourly_rate_usd: rate,
                ssh_key: None,
            }
        }
        ProviderKind::Ssh => {
            let raw_host = args.ssh_host.ok_or_else(|| {
                anyhow::anyhow!("--ssh-host <user@host> is required when --provider ssh")
            })?;
            let (user, host) = match raw_host.split_once('@') {
                Some((u, h)) => (u.to_string(), h.to_string()),
                None => ("user".to_string(), raw_host),
            };
            WorkerConfig::Ssh {
                host,
                user,
                port: args.ssh_port,
                ssh_key: args.ssh_key,
                hourly_rate_usd: 0.0,
            }
        }
    };

    global_cfg
        .workers
        .insert(args.worker_name.clone(), worker_cfg.clone());
    if global_cfg.default_worker.is_none() {
        global_cfg.default_worker = Some(args.worker_name.clone());
    }

    job.stage = "config".into();
    let proj_path = save_project_config(cwd, &project_cfg)?;
    let glob_path = save_global_config(&global_cfg)?;

    if args.bootstrap {
        let provider = create_provider(&worker_cfg);
        job.stage = "worker".into();
        let ep = provider.ensure_running()?;
        job.stage = "preflight".into();
        let probe = preflight::check_tools(&ep)?;
        if probe.missing {
            job.stage = "bootstrap".into();
            let started = Instant::now();
            let result = apple::sdk::bootstrap_worker(provider.as_ref(), &ep, &probe.arch);
            job.bootstrap_ms = started.elapsed().as_millis() as u64;
            result?;
        }
    }

    if json_out {
        job.details
            .insert("project_config".into(), json!(proj_path));
        job.details.insert("global_config".into(), json!(glob_path));
        job.details
            .insert("project_settings".into(), json!(project_cfg));
    } else {
        println!("Initialized Boop for '{}'", project_cfg.project.name);
        println!("  Project config: {}", proj_path.display());
        println!("  Global config:  {}", glob_path.display());
        println!("  Engine:         {}", project_cfg.project.engine);
        if let Some(ref b) = project_cfg.project.bundle_id {
            println!("  Bundle ID:      {b}");
        }
        if let Some(ref d) = project_cfg.project.default_device {
            println!("  Default device: {d}");
        }
        if !project_cfg.metal.bundles.is_empty() {
            println!(
                "  Metal overlays: {} bundle(s) will compile locally (<0.5s cached)",
                project_cfg.metal.bundles.len()
            );
        }
    }
    Ok(())
}

struct BuildContext<'a> {
    root: &'a Path,
    provider: &'a dyn WorkerProvider,
    ep: &'a WorkerEndpoint,
    ws: &'a str,
    stream: bool,
}
impl BuildContext<'_> {
    fn build_stamps(&self, operation: &str, job: &mut JobRecord) -> Result<()> {
        job.stage = "cache".into();
        let script = include_str!("../scripts/pool-build-stamps.py");
        let started = Instant::now();
        let result = self.provider.exec_stream(
            self.ep,
            &format!("python3 - {operation} '{}' <<'PY'\n{script}\nPY", self.ws),
            false,
        )?;
        ensure!(
            result.exit_code == 0,
            "build timestamp {operation}: {}",
            result.output
        );
        let count: usize = result
            .output
            .trim()
            .parse()
            .context("invalid build timestamp count")?;
        job.details.insert(
            format!("stamps_{operation}_ms"),
            json!(started.elapsed().as_millis()),
        );
        job.details
            .insert(format!("stamps_{operation}_count"), json!(count));
        Ok(())
    }

    fn sync(&self, excludes: &[String], skip: bool, job: &mut JobRecord) -> Result<()> {
        job.stage = "snapshot".into();
        let snap = compute_snapshot(self.root, excludes)?;
        job.worktree_id = snap.worktree_id;
        job.snapshot_digest = snap.digest;
        job.details
            .insert("file_count".into(), json!(snap.file_count));
        if !skip {
            job.stage = "sync".into();
            let stats = self
                .provider
                .sync_push(self.ep, self.root, self.ws, &snap.excludes)?;
            job.sync_ms = stats.duration_ms;
        }
        Ok(())
    }
    fn build(
        &mut self,
        project: &config::ProjectConfig,
        args: &BuildArgs,
        job: &mut JobRecord,
    ) -> Result<Option<PathBuf>> {
        self.sync(&project.sync.exclude, false, job)?;
        job.stage = "build".into();
        job.details
            .insert("build_started_at".into(), json!(Utc::now()));
        let result = engine::run_build(self.provider, self.ep, project, self.ws, args, self.stream);
        job.details
            .insert("build_finished_at".into(), json!(Utc::now()));
        let res = result?;
        job.build_ms = res.duration_ms;
        job.exit_code = res.exit_code;
        job.log(&res.output)?;
        if res.exit_code != 0 {
            job.diagnostics = res.diagnostics;
            let last = res
                .output
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("worker returned no output");
            bail!(
                "remote build exited {}: {}",
                res.exit_code,
                job.diagnostics.first().map(String::as_str).unwrap_or(last)
            );
        }
        let Some(remote) = res.remote_artifact_path else {
            return Ok(None);
        };
        let local = self.root.join(".boop/artifacts").join(
            Path::new(&remote)
                .file_name()
                .context("artifact name missing")?,
        );
        job.stage = "pull".into();
        job.pull_ms = self.provider.sync_pull(self.ep, &remote, &local)?;
        job.stage = "package".into();
        apple::bundle::validate(&local, args.target, |missing| {
            let script = include_str!("../scripts/static-frameworks.py");
            let cmd = format!(
                "cd {} && python3 - {} {} <<'BOOP_PY'\n{script}\nBOOP_PY",
                shell_escape(self.ws),
                shell_escape(&serde_json::to_string(missing)?),
                if args.target == BuildTarget::Simulator {
                    "simulator"
                } else {
                    "device"
                }
            );
            let found = self.provider.exec_stream(self.ep, &cmd, false)?;
            ensure!(
                found.exit_code == 0,
                "resolving vendor framework linkage: {}",
                found.output
            );
            Ok(serde_json::from_str(&found.output)?)
        })?;
        job.artifact_path = Some(local.display().to_string());
        job.estimated_cost_usd =
            (job.sync_ms + job.build_ms + job.pull_ms) as f64 / 3600000. * self.ep.hourly_rate_usd;
        Ok(Some(local))
    }

    fn exec(
        &self,
        args: ExecArgs,
        project: Option<&config::ProjectConfig>,
        job: &mut JobRecord,
    ) -> Result<()> {
        self.sync(project.map_or(&[], |p| &p.sync.exclude), args.no_sync, job)?;
        job.stage = "exec".into();
        let joined = args
            .cmd
            .iter()
            .map(|s| shell_escape(s))
            .collect::<Vec<_>>()
            .join(" ");
        let persistent_cache = if self.ws.starts_with("/home/user/boop-projects/") {
            "export XDG_CACHE_HOME=\"$HOME/boop-cache\"; "
        } else {
            ""
        };
        let script = format!(
            "export PATH=\"$HOME/.local/share/swiftly/bin:$HOME/.local/bin:$HOME/.cargo/bin:$PATH\"; {persistent_cache}mkdir -p '{}' && cd '{}' && {joined}",
            self.ws, self.ws
        );
        let res = self.provider.exec_stream(self.ep, &script, self.stream)?;
        job.exit_code = res.exit_code;
        job.log(&res.output)?;
        job.details
            .insert("duration_ms".into(), json!(res.duration_ms));
        job.details.insert("output".into(), json!(res.output));
        ensure!(
            res.exit_code == 0,
            "remote command exited {}: {}",
            res.exit_code,
            res.output.trim()
        );
        Ok(())
    }
}

enum RunTarget {
    Physical(apple::device::PhysicalDevice),
    Simulator(apple::device::SimDevice),
}
impl RunTarget {
    fn resolve(args: &RunArgs, p: &config::ProjectConfig) -> Result<Self> {
        ensure!(
            p.project.bundle_id.is_some(),
            "project.bundle_id is required for boop run"
        );
        if let Some(s) = &args.simulator {
            Ok(Self::Simulator(apple::device::resolve_simulator(s)?))
        } else {
            Ok(Self::Physical(apple::device::resolve_physical_device(
                args.device
                    .as_deref()
                    .filter(|s| *s != "auto")
                    .or(p.project.default_device.as_deref()),
            )?))
        }
    }
    fn platform(&self) -> BuildTarget {
        match self {
            Self::Physical(_) => BuildTarget::Device,
            Self::Simulator(_) => BuildTarget::Simulator,
        }
    }
    fn deploy(
        &self,
        args: &RunArgs,
        p: &config::ProjectConfig,
        root: &Path,
        app: &Path,
        job: &mut JobRecord,
    ) -> Result<()> {
        let bundle = p
            .project
            .bundle_id
            .as_deref()
            .context("missing bundle id")?;
        job.stage = "metal".into();
        if !args.no_metal {
            apple::assets::apply_metal_overlays(root, app, &p.metal.bundles, self.platform())?;
        }
        job.stage = "assets".into();
        apple::assets::apply_asset_overlays(root, app, &p.assets, self.platform())?;
        let start = Instant::now();
        job.stage = "sign".into();
        match self {
            Self::Physical(d) if p.signing.mode == "local-macos" => {
                apple::sign::sign_for_device(
                    app,
                    bundle,
                    p.project
                        .team_id
                        .as_deref()
                        .context("missing project.team_id")?,
                    Some(&d.udid),
                    p.signing.identity.as_deref(),
                    p.signing.provisioning_profile.as_deref(),
                )?;
            }
            Self::Simulator(_) => apple::sign::sign_for_simulator(app)?,
            _ => {}
        }
        job.sign_ms = start.elapsed().as_millis() as u64;
        let start = Instant::now();
        job.stage = "install".into();
        job.device = Some(match self {
            Self::Physical(d) => {
                apple::device::install_and_launch_device(d, app, bundle, !args.no_launch)?;
                d.name.clone()
            }
            Self::Simulator(s) => {
                apple::device::install_and_launch_simulator(s, app, bundle, !args.no_launch)?;
                s.name.clone()
            }
        });
        job.install_ms = start.elapsed().as_millis() as u64;
        Ok(())
    }
}

fn read_git_meta(root: &Path) -> (Option<String>, Option<String>, bool) {
    let branch = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty() && s != "HEAD");
    let commit = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    let dirty = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false);
    (branch, commit, dirty)
}

fn split_csv(raw: Option<&str>, defaults: &[String]) -> Vec<String> {
    if let Some(s) = raw {
        s.split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        defaults.to_vec()
    }
}

fn resolve_publish_signing_kind(
    cli: Option<PublishSigningProfile>,
    cfg: Option<&str>,
) -> Result<apple::sign::SigningProfileKind> {
    if let Some(c) = cli {
        return Ok(match c {
            PublishSigningProfile::Auto => apple::sign::SigningProfileKind::Auto,
            PublishSigningProfile::Adhoc => apple::sign::SigningProfileKind::AdHoc,
            PublishSigningProfile::Development => apple::sign::SigningProfileKind::Development,
        });
    }
    match cfg.unwrap_or("auto").to_lowercase().as_str() {
        "auto" => Ok(apple::sign::SigningProfileKind::Auto),
        "adhoc" | "ad-hoc" => Ok(apple::sign::SigningProfileKind::AdHoc),
        "development" | "dev" => Ok(apple::sign::SigningProfileKind::Development),
        other => bail!(
            "Invalid distribution.firebase.signing_profile '{other}' (expected auto, adhoc, or development)"
        ),
    }
}

fn publish_app_bundle(
    args: &PublishArgs,
    project: &config::ProjectConfig,
    root: &Path,
    app: &Path,
    stream: bool,
    job: &mut JobRecord,
) -> Result<()> {
    let bundle_id = project
        .project
        .bundle_id
        .as_deref()
        .context("project.bundle_id is required for `boop publish`")?;
    let team_id = project
        .project
        .team_id
        .as_deref()
        .context("project.team_id is required for `boop publish`")?;

    let fb_cfg = project.distribution.firebase.as_ref();
    let app_id = args
        .app
        .clone()
        .or_else(|| fb_cfg.and_then(|c| c.app_id.clone()))
        .or_else(|| detect_firebase_app_id(root))
        .context(
            "Missing Firebase App ID. Set `--app <app_id>`, configure `[distribution.firebase].app_id` \
             in .boop.toml, or add GoogleService-Info.plist to the project.",
        )?;

    let groups = split_csv(
        args.groups.as_deref(),
        fb_cfg.map(|c| c.groups.as_slice()).unwrap_or(&[]),
    );
    let testers = split_csv(
        args.testers.as_deref(),
        fb_cfg.map(|c| c.testers.as_slice()).unwrap_or(&[]),
    );
    let signing_kind = resolve_publish_signing_kind(
        args.signing_profile,
        fb_cfg.map(|c| c.signing_profile.as_str()),
    )?;

    let (branch, commit, dirty) = read_git_meta(root);
    let build_id = BuildRecord::new_id();
    let name = args
        .name
        .clone()
        .or_else(|| branch.clone())
        .unwrap_or_else(|| project.project.name.clone());
    let notes = if let Some(path) = &args.notes_file {
        Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read --notes-file {path}"))?,
        )
    } else {
        args.notes.clone()
    };

    job.stage = "metal".into();
    if !args.no_metal {
        apple::assets::apply_metal_overlays(
            root,
            app,
            &project.metal.bundles,
            BuildTarget::Device,
        )?;
    }

    job.stage = "assets".into();
    apple::assets::apply_asset_overlays(root, app, &project.assets, BuildTarget::Device)?;

    job.stage = "version".into();
    let (display_version, build_version) =
        apple::ipa::stamp_build_version(app, args.build_number.as_deref())?;

    let sign_start = Instant::now();
    job.stage = "sign".into();
    let explicit_identity = fb_cfg
        .and_then(|c| c.identity.as_deref())
        .or(project.signing.identity.as_deref());
    let explicit_profile = fb_cfg
        .and_then(|c| c.provisioning_profile.as_deref())
        .or(project.signing.provisioning_profile.as_deref());

    let matched = apple::sign::sign_for_distribution(
        app,
        bundle_id,
        team_id,
        None,
        explicit_identity,
        explicit_profile,
        signing_kind,
    )?;
    job.sign_ms = sign_start.elapsed().as_millis() as u64;

    if signing_kind == apple::sign::SigningProfileKind::Auto
        && matched.kind == apple::sign::ProfileType::Development
    {
        eprintln!(
            "Note: Signed with Development profile '{}' (no Ad Hoc profile installed). \
             OTA installs require a registered device with Developer Mode; install an Apple Distribution \
             cert + Ad Hoc profile for full Firebase OTA compatibility.",
            matched.name
        );
    }

    let pkg_start = Instant::now();
    job.stage = "package".into();
    let app_stem = app
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&project.project.name);
    let ipa_path = root
        .join(".boop/artifacts")
        .join(format!("{app_stem}-{build_id}.ipa"));
    let ipa_bytes = apple::ipa::package_ipa(app, &ipa_path)?;
    job.package_ms = pkg_start.elapsed().as_millis() as u64;

    job.stage = "upload".into();
    let formatted_notes = distribute::format_release_notes(
        &name,
        notes.as_deref(),
        branch.as_deref(),
        commit.as_deref(),
        dirty,
        &job.worktree_id,
        &build_id,
    );
    let upload_res = distribute::upload_firebase(&distribute::FirebasePublishRequest {
        ipa_path: &ipa_path,
        app_id: &app_id,
        release_notes: &formatted_notes,
        groups: &groups,
        testers: &testers,
        stream,
    })?;
    job.upload_ms = upload_res.duration_ms;
    job.log(&upload_res.raw_output)?;

    let final_display = upload_res
        .parsed
        .display_version
        .clone()
        .unwrap_or(display_version);
    let final_build = upload_res
        .parsed
        .build_version
        .clone()
        .unwrap_or(build_version);

    let rec = BuildRecord {
        id: build_id.clone(),
        job_id: job.id.clone(),
        project: project.project.name.clone(),
        name: name.clone(),
        notes,
        worktree_id: job.worktree_id.clone(),
        worktree_path: root.display().to_string(),
        branch,
        commit,
        dirty,
        snapshot_digest: job.snapshot_digest.clone(),
        sandbox_id: job.sandbox_id.clone(),
        config: args.config.as_str().into(),
        bundle_id: Some(bundle_id.to_string()),
        display_version: Some(final_display.clone()),
        build_version: Some(final_build.clone()),
        signing_kind: Some(matched.kind.as_str().into()),
        artifact_path: Some(app.display().to_string()),
        ipa_path: Some(ipa_path.display().to_string()),
        provider: Some("firebase".into()),
        firebase_app_id: Some(app_id),
        testing_uri: upload_res.parsed.testing_uri.clone(),
        firebase_console_uri: upload_res.parsed.firebase_console_uri.clone(),
        created_at: Utc::now(),
    };
    record_build(&rec)?;

    job.details.insert("build_id".into(), json!(build_id));
    job.details.insert("name".into(), json!(name));
    job.details
        .insert("display_version".into(), json!(final_display));
    job.details
        .insert("build_version".into(), json!(final_build));
    job.details
        .insert("signing_kind".into(), json!(matched.kind.as_str()));
    job.details
        .insert("signing_profile".into(), json!(matched.name));
    job.details
        .insert("ipa_path".into(), json!(ipa_path.display().to_string()));
    job.details.insert("ipa_bytes".into(), json!(ipa_bytes));
    if let Some(res) = upload_res.parsed.upload_result {
        job.details.insert("upload_result".into(), json!(res));
    }
    if let Some(uri) = upload_res.parsed.testing_uri {
        job.details.insert("testing_uri".into(), json!(uri));
    }
    if let Some(uri) = upload_res.parsed.firebase_console_uri {
        job.details
            .insert("firebase_console_uri".into(), json!(uri));
    }
    if let Some(uri) = upload_res.parsed.binary_download_uri {
        job.details.insert("binary_download_uri".into(), json!(uri));
    }
    Ok(())
}

fn cmd_builds(args: BuildsArgs, json_out: bool, job: &mut JobRecord) -> Result<()> {
    job.stage = "builds".into();
    let mut builds = load_builds()?;
    if let Some(ref proj) = args.project {
        builds.retain(|b| b.project.eq_ignore_ascii_case(proj));
    }
    if args.published_only {
        builds.retain(|b| b.testing_uri.is_some());
    }
    builds.truncate(args.limit);

    if json_out {
        job.details.insert("builds".into(), json!(builds));
        return Ok(());
    }

    if builds.is_empty() {
        println!("No builds recorded yet. Run `boop publish --name \"...\"` to publish one.");
        return Ok(());
    }

    println!(
        "{:<27} {:<10} {:<24} {:<22} {:<12} TESTING URL",
        "BUILD ID", "PROJECT", "NAME", "VERSION", "SIGNING"
    );
    for b in &builds {
        let ver = match (&b.display_version, &b.build_version) {
            (Some(d), Some(v)) => format!("{d} ({v})"),
            (None, Some(v)) => v.clone(),
            _ => "-".into(),
        };
        let sign = b.signing_kind.as_deref().unwrap_or("unsigned");
        let url = b.testing_uri.as_deref().unwrap_or("(local only)");
        let name_col: String = b.name.chars().take(23).collect();
        println!(
            "{:<27} {:<10} {:<24} {:<22} {:<12} {}",
            b.id, b.project, name_col, ver, sign, url
        );
    }
    Ok(())
}

fn cmd_status(
    cwd: &Path,
    worker_override: Option<&str>,
    json_out: bool,
    job: &mut JobRecord,
) -> Result<()> {
    let global = load_global_config()?;
    let project = find_project_root(cwd).and_then(|r| load_project_config(&r).ok());
    let jobs = load_jobs()?;

    let mut statuses = Vec::new();
    for (name, cfg) in &global.workers {
        if let Some(filter) = worker_override
            && name != filter
        {
            continue;
        }
        let prov = create_provider(cfg);
        let st = prov.status().unwrap_or_else(|e| provider::WorkerStatus {
            provider: prov.provider_name().into(),
            id: name.clone(),
            state: format!("error: {e}"),
            vcpu: None,
            memory_gb: None,
            ssh_endpoint: None,
            usage_summary: None,
        });
        statuses.push((name.clone(), st));
    }

    if json_out {
        let recent: Vec<_> = jobs
            .jobs
            .into_iter()
            .rev()
            .filter(|j| matches!(j.command.as_str(), "build" | "run" | "exec" | "pool warm"))
            .take(10)
            .collect();
        job.project = project.map(|p| p.project.name).unwrap_or_default();
        job.details.insert("workers".into(), json!(statuses));
        job.details.insert("recent_jobs".into(), json!(recent));
        return Ok(());
    }

    println!("Workers:");
    if statuses.is_empty() {
        println!("  (none configured — run `boop init`)");
    }
    for (name, st) in &statuses {
        println!(
            "  {} [{} / {}] state={} ({} vCPU, {} GB)",
            name,
            st.provider,
            st.id,
            st.state,
            st.vcpu.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
            st.memory_gb
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".into())
        );
        if let Some(ref u) = st.usage_summary {
            for line in u.lines().take(2) {
                println!("    {line}");
            }
        }
    }

    let recent: Vec<&JobRecord> = jobs.jobs.iter().rev().take(5).collect();
    if !recent.is_empty() {
        println!("\nRecent jobs:");
        for j in recent {
            println!(
                "  {}  {:<5}  {:<12}  {:<9}  {:.2}s  snap:{}",
                j.id,
                j.command,
                j.project,
                j.status,
                j.total_ms as f64 / 1000.0,
                &j.snapshot_digest[..8.min(j.snapshot_digest.len())]
            );
        }
    }
    Ok(())
}

fn cmd_doctor(
    cwd: &Path,
    worker_override: Option<&str>,
    json_out: bool,
    job: &mut JobRecord,
) -> Result<()> {
    let mut checks: Vec<(&str, bool, String)> = Vec::new();

    // 1. Xcode
    let xcode = std::process::Command::new("xcodebuild")
        .arg("-version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .replace('\n', " ")
                .trim()
                .to_string()
        });
    checks.push((
        "local_xcode",
        xcode.is_some(),
        xcode.unwrap_or_else(|| "xcodebuild not found".into()),
    ));

    // 2. Codesign identities
    let ids = apple::sign::list_codesign_identities().unwrap_or_default();
    checks.push((
        "codesign_identities",
        !ids.is_empty(),
        format!("{} valid signing identity(ies) in Keychain", ids.len()),
    ));

    // 3. Physical iPhones
    let devs = apple::device::list_physical_devices().unwrap_or_default();
    let dev_summary = if devs.is_empty() {
        "No paired physical devices found".into()
    } else {
        devs.iter()
            .map(|d| format!("{} [{}]", d.name, d.state))
            .collect::<Vec<_>>()
            .join(", ")
    };
    checks.push(("physical_devices", !devs.is_empty(), dev_summary));

    // 4. Provisioning profile (if inside a configured project)
    if let Some(root) = find_project_root(cwd)
        && let Ok(proj) = load_project_config(&root)
        && let (Some(bid), Some(tid)) = (
            proj.project.bundle_id.as_deref(),
            proj.project.team_id.as_deref(),
        )
    {
        match apple::sign::find_best_profile(bid, tid, None, &ids, proj.signing.identity.as_deref())
        {
            Ok(m) => checks.push((
                "provisioning_profile",
                true,
                format!("{} (cert {}..)", m.name, &m.cert_sha1[..8]),
            )),
            Err(e) => checks.push(("provisioning_profile", false, e.to_string())),
        }

        let fb_cfg = proj.distribution.firebase.as_ref();
        let dist_id = fb_cfg
            .and_then(|c| c.identity.as_deref())
            .or(proj.signing.identity.as_deref());
        match apple::sign::find_best_profile_with_kind(
            bid,
            tid,
            None,
            &ids,
            dist_id,
            apple::sign::SigningProfileKind::AdHoc,
        ) {
            Ok(m) => checks.push((
                "adhoc_profile",
                true,
                format!("{} [{}] (cert {}..)", m.name, m.kind.as_str(), &m.cert_sha1[..8]),
            )),
            Err(_) => checks.push((
                "adhoc_profile",
                false,
                format!(
                    "No Ad Hoc distribution profile for {tid}.{bid} (`boop publish` will fall back to Development in auto mode)"
                ),
            )),
        }
    }

    // 4b. Firebase CLI (for `boop publish`)
    let (fb_ok, fb_msg) = distribute::check_firebase_cli();
    checks.push(("firebase_cli", fb_ok, fb_msg));

    // 5. Remote worker health
    if let Ok(global) = load_global_config() {
        let proj = find_project_root(cwd).and_then(|r| load_project_config(&r).ok());
        if let Ok((name, cfg)) = resolve_worker(
            &global,
            worker_override,
            proj.as_ref().and_then(|p| p.project.worker.as_deref()),
        ) {
            let prov = create_provider(cfg);
            match prov.status() {
                Ok(st) => {
                    let ok = matches!(
                        st.state.as_str(),
                        "idle" | "running" | "ready" | "reachable" | "stopped"
                    );
                    checks.push((
                        "worker",
                        ok,
                        format!("{name} ({}/{}) state={}", st.provider, st.id, st.state),
                    ));
                    if matches!(
                        st.state.as_str(),
                        "idle" | "running" | "ready" | "reachable"
                    ) && let Ok(ep) = prov.ensure_running()
                    {
                        let tc = prov.exec_stream(
                                &ep,
                                "export PATH=\"$HOME/.local/share/swiftly/bin:$HOME/.local/bin:$PATH\"; swift --version | head -n1 && xtool2 --version && xtool2 sdk status | head -n1",
                                false,
                            );
                        match tc {
                            Ok(r) if r.exit_code == 0 => checks.push((
                                "remote_toolchain",
                                true,
                                r.output.lines().collect::<Vec<_>>().join(" | "),
                            )),
                            Ok(r) => {
                                checks.push(("remote_toolchain", false, r.output.trim().into()))
                            }
                            Err(e) => checks.push(("remote_toolchain", false, e.to_string())),
                        }
                    }
                }
                Err(e) => checks.push(("worker", false, e.to_string())),
            }
        }
    }

    if json_out {
        job.details.insert("checks".into(), json!(checks));
    } else {
        for (name, ok, msg) in &checks {
            let icon = if *ok { "OK  " } else { "FAIL" };
            println!("[{icon}] {name:<22} {msg}");
        }
    }
    Ok(())
}

fn shell_escape(s: &str) -> String {
    if !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '=' | ':' | ',')
        })
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ExecResult, SyncStats, WorkerStatus};
    use tempfile::tempdir;

    struct MockProvider {
        exit_code: i32,
        output: String,
    }

    impl WorkerProvider for MockProvider {
        fn provider_name(&self) -> &'static str {
            "mock"
        }
        fn ensure_running(&self) -> Result<WorkerEndpoint> {
            Ok(WorkerEndpoint {
                provider: "mock".into(),
                hydrating: false,
                id: "mock-1".into(),
                host: "127.0.0.1".into(),
                port: 22,
                user: "user".into(),
                ssh_key: None,
                hourly_rate_usd: 0.036,
            })
        }
        fn stop(&self) -> Result<()> {
            Ok(())
        }
        fn status(&self) -> Result<WorkerStatus> {
            Ok(WorkerStatus {
                provider: "mock".into(),
                id: "mock-1".into(),
                state: "ready".into(),
                vcpu: Some(4),
                memory_gb: Some(8),
                ssh_endpoint: None,
                usage_summary: None,
            })
        }
        fn sync_push(
            &self,
            _ep: &WorkerEndpoint,
            _local: &Path,
            _remote: &str,
            _excludes: &[String],
        ) -> Result<SyncStats> {
            Ok(SyncStats {
                files_transferred: 3,
                bytes_sent: 4096,
                duration_ms: 42,
            })
        }
        fn sync_pull(&self, _ep: &WorkerEndpoint, _remote: &str, local: &Path) -> Result<u64> {
            std::fs::create_dir_all(local)?;
            Ok(15)
        }
        fn exec_stream(
            &self,
            _ep: &WorkerEndpoint,
            _cmd: &str,
            _stream: bool,
        ) -> Result<ExecResult> {
            Ok(ExecResult {
                exit_code: self.exit_code,
                output: self.output.clone(),
                duration_ms: 120,
            })
        }
    }

    #[test]
    fn pipeline_rejects_an_empty_pulled_artifact() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("App.swift"), "struct App {}").unwrap();
        let proj = config::ProjectConfig {
            project: config::ProjectSection {
                name: "Demo".into(),
                engine: "xtool".into(),
                scheme: Some("Demo".into()),
                app_name: Some("Demo".into()),
                bundle_id: Some("com.example.demo".into()),
                team_id: Some("TEAM".into()),
                worker: None,
                default_device: None,
                compilation_cache: false,
            },
            sync: config::SyncSection::default(),
            signing: config::SigningSection::default(),
            metal: config::MetalSection::default(),
            assets: config::AssetsSection::default(),
            distribution: config::DistributionSection::default(),
        };
        let prov = MockProvider {
            exit_code: 0,
            output: "[100%] Build completed.\n".into(),
        };
        let ep = prov.ensure_running().unwrap();
        let build = BuildArgs {
            config: cli::BuildConfig::Debug,
            target: BuildTarget::Device,
            scheme: None,
            clean: false,
            verbose: false,
            name: None,
            notes: None,
        };
        let mut job = JobRecord::new("build");
        let mut ctx = BuildContext {
            root: dir.path(),
            provider: &prov,
            ep: &ep,
            ws: "/home/user/test",
            stream: false,
        };
        let result = ctx.build(&proj, &build, &mut job);
        assert!(result.is_err()); // Mock pull supplies an empty .app.
        assert_eq!(job.stage, "package");
        assert_eq!(job.pull_ms, 15);
        assert!(job.artifact_path.is_none());
    }

    #[test]
    fn pipeline_records_failure_diagnostics_without_local_fallback() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("App.swift"), "broken").unwrap();
        let proj = config::ProjectConfig {
            project: config::ProjectSection {
                name: "Demo".into(),
                engine: "xtool".into(),
                scheme: Some("Demo".into()),
                app_name: Some("Demo".into()),
                bundle_id: None,
                team_id: None,
                worker: None,
                default_device: None,
                compilation_cache: false,
            },
            sync: config::SyncSection::default(),
            signing: config::SigningSection::default(),
            metal: config::MetalSection::default(),
            assets: config::AssetsSection::default(),
            distribution: config::DistributionSection::default(),
        };
        let prov = MockProvider {
            exit_code: 1,
            output: "/ws/App.swift:1:1: error: expected declaration\n".into(),
        };
        let ep = prov.ensure_running().unwrap();
        let build = BuildArgs {
            config: cli::BuildConfig::Debug,
            target: BuildTarget::Device,
            scheme: None,
            clean: false,
            verbose: false,
            name: None,
            notes: None,
        };
        let mut job = JobRecord::new("build");
        let mut ctx = BuildContext {
            root: dir.path(),
            provider: &prov,
            ep: &ep,
            ws: "/home/user/test",
            stream: false,
        };
        let err = ctx.build(&proj, &build, &mut job).unwrap_err();
        job.fail(&err);
        assert_eq!(job.failure_kind.as_deref(), Some("code"));
        assert_eq!(job.diagnostics.len(), 1);
    }
}
