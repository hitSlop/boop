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

    // Non-destructive remote-only project patcher: disables remote codesigning across all
    // buildSettings blocks (Automatic, Manual, omitted, or Xcode 27.2 JSON) so xtool 2.0 beta's
    // in-process CodesignTool doesn't choke on codeless xcframeworks (signing happens locally on Mac),
    // optionally enables Swift CAS, and preserves final mtimes across unchanged SHA-256 outputs.
    let cas_setup = if project.project.compilation_cache {
        "mkdir -p \"$HOME/boop-cache/swift-cas\"\n"
    } else {
        ""
    };
    let mut patch_flags = vec![
        "--stamp-file".to_string(),
        "xtool/.boop-project-inputs.json".to_string(),
    ];
    if project.signing.disable_remote_codesign {
        patch_flags.push("--disable-codesign".to_string());
    }
    if project.project.compilation_cache {
        patch_flags.push("--cas-dir".to_string());
        patch_flags.push("$HOME/boop-cache/swift-cas".to_string());
    }
    let patch_flag_str = patch_flags
        .iter()
        .map(|flag| {
            if flag.starts_with("$HOME/") {
                format!("\"{flag}\"")
            } else {
                crate::shell_escape(flag)
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let patch_script = include_str!("../../scripts/patch-xcodeproj.py");

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
{clean_step}{cas_setup}python3 - {patch_flag_str} <<'BOOP_PATCH_PY'
{patch_script}
BOOP_PATCH_PY
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    fn run_patcher(dir: &Path, extra_args: &[&str]) {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/patch-xcodeproj.py");
        let out = Command::new("python3")
            .arg(&script)
            .args(extra_args)
            .current_dir(dir)
            .output()
            .expect("python3 should execute patch-xcodeproj.py");
        assert!(
            out.status.success(),
            "patch-xcodeproj.py failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn patches_openstep_automatic_manual_and_omitted_signing() {
        let dir = tempdir().unwrap();
        let proj = dir.path().join("Sample.xcodeproj");
        fs::create_dir_all(&proj).unwrap();
        let pbx = proj.join("project.pbxproj");
        let original = r#"// !$*UTF8*$!
{
	archiveVersion = 1;
	objects = {
		AUTO01 = {
			isa = XCBuildConfiguration;
			buildSettings = {
				CODE_SIGN_STYLE = Automatic;
				DEVELOPMENT_TEAM = TEAM123456;
				PRODUCT_BUNDLE_IDENTIFIER = com.example.auto;
			};
			name = Debug;
		};
		MANUAL01 = {
			isa = XCBuildConfiguration;
			buildSettings = {
				CODE_SIGN_STYLE = Manual;
				"CODE_SIGN_IDENTITY[sdk=iphoneos*]" = "Apple Distribution";
				PROVISIONING_PROFILE_SPECIFIER = "My AdHoc Profile";
				DEVELOPMENT_TEAM = TEAM123456;
				OTHER_LDFLAGS = (
					"-ObjC",
					"/* not a brace { */",
				);
			};
			name = Release;
		};
		OMITTED01 = {
			isa = XCBuildConfiguration;
			buildSettings = {
				CODE_SIGN_IDENTITY = "iPhone Developer";
				PRODUCT_BUNDLE_IDENTIFIER = com.example.omitted;
			};
			name = Debug;
		};
	};
}
"#;
        fs::write(&pbx, original).unwrap();

        run_patcher(dir.path(), &["--disable-codesign"]);
        let patched = fs::read_to_string(&pbx).unwrap();

        let count_line = |needle: &str| patched.lines().filter(|l| l.trim() == needle).count();
        assert_eq!(count_line("CODE_SIGNING_ALLOWED = NO;"), 3);
        assert_eq!(count_line("CODE_SIGNING_REQUIRED = NO;"), 3);
        assert_eq!(count_line("AD_HOC_CODE_SIGNING_ALLOWED = NO;"), 3);
        assert_eq!(count_line("CODE_SIGN_STYLE = Manual;"), 3);
        assert_eq!(count_line("CODE_SIGN_IDENTITY = \"\";"), 3);
        assert!(!patched.contains("CODE_SIGN_STYLE = Automatic;"));
        assert!(!patched.contains("Apple Distribution"));
        assert!(!patched.contains("PROVISIONING_PROFILE_SPECIFIER"));
        assert!(!patched.contains("iPhone Developer"));
        // Preserve DEVELOPMENT_TEAM and unrelated settings
        assert_eq!(patched.matches("DEVELOPMENT_TEAM = TEAM123456;").count(), 2);
        assert!(patched.contains("\"/* not a brace { */\""));

        // Idempotency + mtime preservation check
        let first_mtime = fs::metadata(&pbx).unwrap().modified().unwrap();
        thread::sleep(Duration::from_millis(20));
        // Rewrite original (simulating a fresh rsync with a newer mtime) and re-patch
        fs::write(&pbx, original).unwrap();
        run_patcher(dir.path(), &["--disable-codesign"]);
        let second = fs::read_to_string(&pbx).unwrap();
        let second_mtime = fs::metadata(&pbx).unwrap().modified().unwrap();
        assert_eq!(patched, second);
        assert_eq!(first_mtime, second_mtime);
    }

    #[test]
    fn patches_json_xcproj_and_cas_settings() {
        let dir = tempdir().unwrap();
        let proj = dir.path().join("Modern.xcodeproj");
        fs::create_dir_all(&proj).unwrap();
        let xcproj = proj.join("project.xcproj");
        let original = r#"{
  "rootObject": "R1",
  "objects": {
    "CFG1": {
      "isa": "XCBuildConfiguration",
      "name": "Debug",
      "buildSettings": {
        "CODE_SIGN_STYLE": "Manual",
        "CODE_SIGN_IDENTITY[sdk=iphoneos*]": "Apple Development",
        "PROVISIONING_PROFILE": "uuid-123",
        "DEVELOPMENT_TEAM": "TEAM777",
        "PRODUCT_BUNDLE_IDENTIFIER": "com.example.modern"
      }
    }
  }
}
"#;
        fs::write(&xcproj, original).unwrap();

        run_patcher(
            dir.path(),
            &["--disable-codesign", "--cas-dir", "/tmp/test-cas"],
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&xcproj).unwrap()).unwrap();
        let bs = &parsed["objects"]["CFG1"]["buildSettings"];

        assert_eq!(bs["CODE_SIGN_STYLE"], "Manual");
        assert_eq!(bs["CODE_SIGNING_ALLOWED"], "NO");
        assert_eq!(bs["CODE_SIGNING_REQUIRED"], "NO");
        assert_eq!(bs["AD_HOC_CODE_SIGNING_ALLOWED"], "NO");
        assert_eq!(bs["CODE_SIGN_IDENTITY"], "");
        assert!(bs.get("CODE_SIGN_IDENTITY[sdk=iphoneos*]").is_none());
        assert!(bs.get("PROVISIONING_PROFILE").is_none());
        assert_eq!(bs["DEVELOPMENT_TEAM"], "TEAM777");
        assert_eq!(bs["COMPILATION_CACHE_ENABLE_CACHING"], "YES");
        assert_eq!(bs["COMPILATION_CACHE_CAS_PATH"], "/tmp/test-cas");
        assert_eq!(bs["SWIFT_ENABLE_EXPLICIT_MODULES"], "YES");
    }
}
