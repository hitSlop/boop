pub mod cargo;
pub mod xtool;

use crate::cli::BuildArgs;
use crate::config::ProjectConfig;
use crate::provider::{WorkerEndpoint, WorkerProvider};
use anyhow::{Result, bail};

#[derive(Debug, Clone)]
pub struct BuildOutcome {
    pub exit_code: i32,
    pub duration_ms: u64,
    pub remote_artifact_path: Option<String>,
    pub diagnostics: Vec<String>,
    pub output: String,
}

pub fn run_build(
    provider: &dyn WorkerProvider,
    endpoint: &WorkerEndpoint,
    project: &ProjectConfig,
    remote_workspace: &str,
    args: &BuildArgs,
    stream: bool,
) -> Result<BuildOutcome> {
    match project.project.engine.as_str() {
        "xtool" => xtool::build(
            provider,
            endpoint,
            project,
            remote_workspace,
            args.config,
            args.target,
            args.scheme.as_deref(),
            args.verbose,
            args.clean,
            stream,
        ),
        "cargo" => cargo::build(
            provider,
            endpoint,
            remote_workspace,
            args.config,
            args.clean,
            stream,
        ),
        other => {
            bail!("Unsupported build engine '{other}' in .boop.toml (expected 'xtool' or 'cargo')")
        }
    }
}

/// Extract concise compiler error diagnostics from Swift / Clang / Cargo output.
pub fn extract_diagnostics(output: &str) -> Vec<String> {
    let mut diags = Vec::new();
    for line in output.lines() {
        let t = line.trim();
        if t.starts_with("diagnostic(")
            && t.contains("DiagnosticInfo.Kind.error,")
            && let Some((_, rest)) = t.split_once(", message: ")
        {
            let mut messages = serde_json::Deserializer::from_str(rest).into_iter::<String>();
            if let Some(Ok(message)) = messages.next() {
                let diag = format!("Swift Build: error: {message}");
                if !diags.contains(&diag) {
                    diags.push(diag);
                }
                continue;
            }
        }
        if (t.contains(": error:")
            || t.starts_with("error:")
            || t.starts_with("error[E")
            || t.contains("ld64.lld: error:")
            || t.contains("undefined symbol:"))
            && !diags.iter().any(|existing| existing == t)
        {
            diags.push(t.to_string());
        }
    }
    diags.truncate(40);
    diags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_swift_and_linker_errors() {
        let log = "\
warning: unused variable 'x'\n\
/home/user/ws/App.swift:42:12: error: cannot find 'Foo' in scope\n\
/home/user/ws/App.swift:42:12: error: cannot find 'Foo' in scope\n\
error: Build failed\n";
        let diags = extract_diagnostics(log);
        assert_eq!(diags.len(), 2);
        assert!(diags[0].contains("App.swift:42:12: error:"));
    }
}
