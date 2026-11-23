use crate::cli::{BuildConfig, BuildTarget};
use crate::config::ProjectConfig;
use crate::engine::{BuildOutcome, extract_diagnostics};
use crate::provider::{WorkerEndpoint, WorkerProvider};
use anyhow::Result;

#[allow(clippy::too_many_arguments)]
pub fn build(
    provider: &dyn WorkerProvider,
    endpoint: &WorkerEndpoint,
    project: &ProjectConfig,
    remote_workspace: &str,
    config: BuildConfig,
    target: BuildTarget,
    scheme_override: Option<&str>,
    verbose: bool,
    clean: bool,
    stream: bool,
) -> Result<BuildOutcome> {
    let scheme = scheme_override.or(project.project.scheme.as_deref());
    let app_name = project
        .project
        .app_name
        .as_deref()
        .or(scheme)
        .unwrap_or(&project.project.name);

    let mut xtool_args = vec![
        "dev".to_string(),
        "build".to_string(),
        "--configuration".to_string(),
        config.as_str().to_string(),
        "--triple".to_string(),
        target.triple().to_string(),
    ];
    if let Some(s) = scheme {
        xtool_args.push("--scheme".to_string());
        xtool_args.push(s.to_string());
    }
    if verbose {
        xtool_args.push("--verbose".to_string());
    }

    let clean_step = if clean {
        "rm -rf xtool/.xtool-tmp xtool/*.app\n"
    } else {
        ""
    };

    // Non-destructive remote-only pbxproj patch: disables remote codesigning so xtool 2.0 beta's
    // in-process CodesignTool doesn't choke on codeless xcframeworks (signing happens locally on Mac).
    let pbxproj_patch = if project.signing.disable_remote_codesign {
        r#"python3 - <<'PY'
import glob
for p in glob.glob("*.xcodeproj/project.pbxproj"):
    s = open(p).read()
    if "CODE_SIGNING_ALLOWED = NO;" not in s and "CODE_SIGN_STYLE = Automatic;" in s:
        s = s.replace(
            "CODE_SIGN_STYLE = Automatic;",
            "CODE_SIGN_STYLE = Manual;\n\t\t\t\tCODE_SIGNING_ALLOWED = NO;\n\t\t\t\tCODE_SIGNING_REQUIRED = NO;\n\t\t\t\tAD_HOC_CODE_SIGNING_ALLOWED = NO;\n\t\t\t\tCODE_SIGN_IDENTITY = \"\";"
        )
        open(p, "w").write(s)
PY
"#
    } else {
        ""
    };

    let cas_patch = if project.project.compilation_cache {
        r#"mkdir -p "$HOME/boop-cache/swift-cas"
python3 - <<'PY'
import glob, os
cas_dir = os.path.expanduser("~/boop-cache/swift-cas")
for p in glob.glob("*.xcodeproj/project.pbxproj"):
    s = open(p).read()
    if "COMPILATION_CACHE_ENABLE_CACHING" not in s:
        s = s.replace(
            "buildSettings = {",
            f'buildSettings = {{\n\t\t\t\tCOMPILATION_CACHE_ENABLE_CACHING = YES;\n\t\t\t\tCOMPILATION_CACHE_CAS_PATH = "{cas_dir}";\n\t\t\t\tSWIFT_ENABLE_EXPLICIT_MODULES = YES;'
        )
        open(p, "w").write(s)
PY
"#
    } else {
        ""
    };

    if !std::path::Path::new(app_name)
        .file_name()
        .is_some_and(|s| s == app_name)
    {
        anyhow::bail!("invalid app bundle name: {app_name}");
    }
    let xtool_cmd = format!(
        "xtool2 {}",
        xtool_args
            .iter()
            .map(|s| crate::shell_escape(s))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let cache = if remote_workspace.starts_with("/home/user/boop-projects/") {
        include_str!("../../scripts/pool-cache.sh")
    } else {
        ""
    };
    let remote_script = format!(
        r#"set -euo pipefail
export PATH="$HOME/.local/share/swiftly/bin:$HOME/.local/bin:$PATH"
cd '{remote_workspace}'
mkdir -p "$HOME/.boop-locks"
exec 9>"$HOME/.boop-locks/$(basename '{remote_workspace}').lock"
flock 9
{cache}
{clean_step}{pbxproj_patch}{cas_patch}
# Preserve the timestamp of the final patched project across checksum syncs.
# Swift Build keys parts of its dependency graph on the pbxproj timestamp.
python3 - <<'PY'
import glob, hashlib, json, os
from pathlib import Path
cache = Path("xtool/.boop-project-inputs.json")
old = json.loads(cache.read_text()) if cache.is_file() else dict()
current = dict()
for name in glob.glob("*.xcodeproj/project.pbxproj"):
    p = Path(name)
    digest = hashlib.sha256(p.read_bytes()).hexdigest()
    stamp = p.stat().st_mtime_ns
    if name in old and old[name][0] == digest:
        stamp = old[name][1]
        os.utime(p, ns=(p.stat().st_atime_ns, stamp))
    current[name] = [digest, stamp]
cache.parent.mkdir(exist_ok=True)
cache.write_text(json.dumps(current))
PY
{xtool_cmd}
"#
    );

    let res = provider.exec_stream(endpoint, &remote_script, stream)?;
    let diagnostics = if res.exit_code == 0 {
        Vec::new()
    } else {
        extract_diagnostics(&res.output)
    };

    let remote_artifact_path = if res.exit_code == 0 {
        Some(format!(
            "{}/xtool/{app_name}.app",
            remote_workspace.trim_end_matches('/')
        ))
    } else {
        None
    };

    Ok(BuildOutcome {
        exit_code: res.exit_code,
        duration_ms: res.duration_ms,
        remote_artifact_path,
        diagnostics,
        output: res.output,
    })
}
