use crate::cli::BuildConfig;
use crate::engine::{BuildOutcome, extract_diagnostics};
use crate::provider::{WorkerEndpoint, WorkerProvider};
use anyhow::Result;

pub fn build(
    provider: &dyn WorkerProvider,
    endpoint: &WorkerEndpoint,
    remote_workspace: &str,
    config: BuildConfig,
    clean: bool,
    stream: bool,
) -> Result<BuildOutcome> {
    let flag = match config {
        BuildConfig::Debug => "",
        BuildConfig::Release => "--release",
    };
    let clean_step = if clean { "cargo clean\n" } else { "" };
    let script = format!(
        r#"set -euo pipefail
export PATH="$HOME/.cargo/bin:/usr/local/cargo/bin:$PATH"
cd '{remote_workspace}'
{clean_step}cargo build {flag}
"#
    );

    let res = provider.exec_stream(endpoint, &script, stream)?;
    let diagnostics = if res.exit_code == 0 {
        Vec::new()
    } else {
        extract_diagnostics(&res.output)
    };

    Ok(BuildOutcome {
        exit_code: res.exit_code,
        duration_ms: res.duration_ms,
        remote_artifact_path: None,
        diagnostics,
        output: res.output,
    })
}
