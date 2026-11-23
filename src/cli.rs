use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "boop",
    version,
    about = "Build over there. Run right here. Local-first remote iOS & Rust builds."
)]
pub struct Cli {
    /// Emit structured JSON summary at the end of execution (useful for coding agents)
    #[arg(long, global = true)]
    pub json: bool,

    /// Override worker name from ~/.config/boop/config.toml
    #[arg(long, global = true)]
    pub worker: Option<String>,

    /// Return immediately when the requested sandbox is busy or the pool is full
    #[arg(long, global = true)]
    pub no_wait: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Maintain the selected Boat worker pool
    #[command(subcommand)]
    Pool(PoolCommand),

    /// Delete the fork leased to the current worktree
    Release,

    /// Initialize .boop.toml in the current project and register a Boat or SSH worker
    Init(InitArgs),

    /// Sync local worktree snapshot and build remotely, pulling the artifact locally
    Build(BuildArgs),

    /// Build remotely, locally overlay Metal/assets & codesign, then install + launch on iPhone/Simulator
    Run(RunArgs),

    /// Build remotely, locally overlay Metal & codesign, package IPA, and publish to Firebase App Distribution
    Publish(PublishArgs),

    /// List published and named builds in the local build inbox
    Builds(BuildsArgs),

    /// Sync current worktree snapshot and execute an arbitrary command on the remote worker
    Exec(ExecArgs),

    /// Show configured workers, live VM status, recent jobs, and estimated spend
    Status,

    /// Stop/pause the remote worker to halt compute billing (preserves disk & build caches)
    Stop(StopArgs),

    /// Check local Xcode, signing identities, provisioning profiles, devices, and remote worker health
    Doctor,
}

#[derive(clap::Args, Debug)]
pub struct InitArgs {
    /// Existing Boat sandbox ID to bind (e.g. bx_xxxxxxxx). If omitted with --provider boat, creates one.
    #[arg(long)]
    pub boat_id: Option<String>,

    /// Provider type: boat or ssh
    #[arg(long, default_value = "boat")]
    pub provider: ProviderKind,

    /// Machine class for newly created Boat sandboxes (default, large, xlarge, small)
    #[arg(long, default_value = "default")]
    pub machine_type: String,

    /// Direct SSH target (user@host) when --provider ssh
    #[arg(long)]
    pub ssh_host: Option<String>,

    /// SSH port when --provider ssh
    #[arg(long, default_value_t = 22)]
    pub ssh_port: u16,

    /// SSH private key path when --provider ssh
    #[arg(long)]
    pub ssh_key: Option<String>,

    /// Worker alias to save in ~/.config/boop/config.toml
    #[arg(long, default_value = "default")]
    pub worker_name: String,

    /// Also bootstrap Swift 6.4 + xtool + shims and upload slim Darwin SDK if missing
    #[arg(long)]
    pub bootstrap: bool,
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum ProviderKind {
    Boat,
    Ssh,
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum BuildConfig {
    Debug,
    Release,
}

impl BuildConfig {
    pub fn as_str(&self) -> &'static str {
        match self {
            BuildConfig::Debug => "debug",
            BuildConfig::Release => "release",
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum BuildTarget {
    Device,
    Simulator,
}

impl BuildTarget {
    pub fn triple(&self) -> &'static str {
        match self {
            BuildTarget::Device => "arm64-apple-ios",
            BuildTarget::Simulator => "arm64-apple-ios-simulator",
        }
    }
}

#[derive(clap::Args, Debug)]
pub struct BuildArgs {
    /// Build configuration
    #[arg(short, long, default_value = "debug")]
    pub config: BuildConfig,

    /// Target platform (device = arm64-apple-ios, simulator = arm64-apple-ios-simulator)
    #[arg(short, long, default_value = "device")]
    pub target: BuildTarget,

    /// Override Xcode scheme
    #[arg(long)]
    pub scheme: Option<String>,

    /// Pass --verbose to the remote build tool
    #[arg(short, long)]
    pub verbose: bool,

    /// Clean remote build intermediates before building
    #[arg(long)]
    pub clean: bool,

    /// Optional human-readable build name (records the build in `boop builds`)
    #[arg(long)]
    pub name: Option<String>,

    /// Optional build description / summary notes
    #[arg(long)]
    pub notes: Option<String>,
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum PublishSigningProfile {
    Auto,
    Adhoc,
    Development,
}

#[derive(clap::Args, Debug)]
pub struct PublishArgs {
    /// Short build label shown in Firebase release notes and `boop builds` (defaults to git branch)
    #[arg(long)]
    pub name: Option<String>,

    /// Release notes / agent summary for this build
    #[arg(long)]
    pub notes: Option<String>,

    /// Path to a file containing release notes
    #[arg(long)]
    pub notes_file: Option<String>,

    /// Override Firebase App ID (defaults to .boop.toml or GoogleService-Info.plist)
    #[arg(long)]
    pub app: Option<String>,

    /// Comma-separated Firebase tester group aliases (overrides .boop.toml)
    #[arg(long)]
    pub groups: Option<String>,

    /// Comma-separated tester email addresses (overrides .boop.toml)
    #[arg(long)]
    pub testers: Option<String>,

    /// Build configuration (defaults to debug to reuse warm Boat fork caches)
    #[arg(short, long, default_value = "debug")]
    pub config: BuildConfig,

    /// Override Xcode scheme
    #[arg(long)]
    pub scheme: Option<String>,

    /// Signing profile preference: auto (prefer Ad Hoc, fallback Development), adhoc, or development
    #[arg(long)]
    pub signing_profile: Option<PublishSigningProfile>,

    /// Explicit CFBundleVersion to stamp into Info.plist (defaults to UTC YYYYMMDD.HHMMSS)
    #[arg(long)]
    pub build_number: Option<String>,

    /// Skip remote Boat build and package/sign/publish the existing pulled .boop/artifacts/<App>.app
    #[arg(long)]
    pub no_build: bool,

    /// Skip local Metal shader compilation overlay
    #[arg(long)]
    pub no_metal: bool,

    /// Clean remote build intermediates before building
    #[arg(long)]
    pub clean: bool,

    /// Pass --verbose to the remote build tool
    #[arg(short, long)]
    pub verbose: bool,
}

#[derive(clap::Args, Debug)]
pub struct BuildsArgs {
    /// Filter builds by project name
    #[arg(long)]
    pub project: Option<String>,

    /// Maximum number of builds to display
    #[arg(long, default_value_t = 20)]
    pub limit: usize,

    /// Only show builds published to Firebase App Distribution
    #[arg(long)]
    pub published_only: bool,
}

#[derive(clap::Args, Debug)]
pub struct RunArgs {
    /// Deploy to a physical iPhone (optionally pass name or UDID; defaults to auto-detect)
    #[arg(long, num_args = 0..=1, default_missing_value = "auto")]
    pub device: Option<String>,

    /// Deploy to a local iOS Simulator (optionally pass name or UDID; defaults to booted)
    #[arg(long, num_args = 0..=1, default_missing_value = "booted")]
    pub simulator: Option<String>,

    /// Build configuration
    #[arg(short, long, default_value = "debug")]
    pub config: BuildConfig,

    /// Override Xcode scheme
    #[arg(long)]
    pub scheme: Option<String>,

    /// Pass --verbose to the remote build tool
    #[arg(short, long)]
    pub verbose: bool,

    /// Skip local Metal shader compilation overlay
    #[arg(long)]
    pub no_metal: bool,

    /// Skip installing & launching (only build + sign locally)
    #[arg(long)]
    pub no_launch: bool,
}

#[derive(clap::Args, Debug)]
pub struct ExecArgs {
    /// Skip syncing local source before running command
    #[arg(long)]
    pub no_sync: bool,

    /// Command and arguments to execute remotely
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub cmd: Vec<String>,
}

#[derive(clap::Args, Debug)]
pub struct StopArgs {
    /// Worker name to stop (defaults to active worker)
    #[arg(long)]
    pub name: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum PoolCommand {
    /// Build the project on its base and stop it to publish the snapshot
    Warm(BuildArgs),
    /// List the base and forks owned by this pool
    Status,
    /// Delete idle forks whose lease is absent or stale
    Gc,
}
