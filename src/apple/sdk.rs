use crate::config::{cache_dir, home_dir};
use crate::provider::{WorkerEndpoint, WorkerProvider};
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const BOOTSTRAP_SCRIPT: &str = include_str!("../../scripts/bootstrap-linux-worker.sh");

/// Bootstrap Swift 6.4 + xtool 2.0 + shims on the remote Linux worker, and build/upload
/// the slim `darwin.xtoolsdk` from local `/Applications/Xcode.app` if not already installed.
pub fn bootstrap_worker(
    provider: &dyn WorkerProvider,
    ep: &WorkerEndpoint,
    remote_arch: &str,
) -> Result<()> {
    let xtool_arch = match remote_arch {
        "x86_64" => "x86_64",
        "aarch64" | "arm64" => "arm64",
        other => bail!("Unsupported remote Linux architecture: {other}"),
    };

    let env_prefix = local_toolchain_env_prefix();
    eprintln!(
        "==> Bootstrapping Swift 6.4, xtool 2.0, and Linux build shims on {}...",
        ep.id
    );
    let tmp_remote = "/tmp/boop-bootstrap.sh";
    let push_script = format!(
        "cat > '{tmp_remote}' <<'BOOP_EOF'\n{BOOTSTRAP_SCRIPT}\nBOOP_EOF\nchmod +x '{tmp_remote}' && {env_prefix}bash '{tmp_remote}'"
    );
    let boot = provider.exec_stream(ep, &push_script, false)?;
    if boot.exit_code != 0 {
        bail!(
            "Remote bootstrap failed (exit {}): {}",
            boot.exit_code,
            boot.output
        );
    }

    // Check if Darwin SDK is already installed remotely
    let sdk_check = provider.exec_stream(
        ep,
        "export PATH=\"$HOME/.local/share/swiftly/bin:$HOME/.local/bin:$PATH\"; xtool2 sdk status",
        false,
    )?;
    if sdk_check.output.contains("Darwin SDK is installed") {
        eprintln!("==> Darwin SDK already installed on worker.");
        // Re-run bootstrap once more so toolset/bin shims are guaranteed present
        let _ = provider.exec_stream(ep, &format!("{env_prefix}bash '{tmp_remote}'"), false);
        return Ok(());
    }

    let tarball = ensure_local_darwin_sdk_tarball(xtool_arch)?;
    eprintln!(
        "==> Uploading slim Darwin SDK ({}) to worker...",
        tarball.display()
    );

    let mut scp = Command::new("scp");
    scp.args([
        "-P",
        &ep.port.to_string(),
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
    ]);
    if let Some(ref key) = ep.ssh_key {
        scp.args(["-i", key]);
    }
    scp.arg(&tarball);
    scp.arg(format!(
        "{}@{}:/tmp/darwin.xtoolsdk.tar.gz",
        ep.user, ep.host
    ));
    let status = scp
        .stdout(std::process::Stdio::from(std::io::stderr()))
        .status()
        .context("Failed to run scp for Darwin SDK")?;
    if !status.success() {
        bail!("Failed to upload Darwin SDK tarball to worker");
    }

    eprintln!("==> Installing Darwin SDK on worker...");
    let install_cmd = format!(
        r#"set -euo pipefail
export PATH="$HOME/.local/share/swiftly/bin:$HOME/.local/bin:$PATH"
rm -rf /tmp/boop-sdk-extract && mkdir -p /tmp/boop-sdk-extract
tar -xzf /tmp/darwin.xtoolsdk.tar.gz -C /tmp/boop-sdk-extract
find /tmp/boop-sdk-extract -name '._*' -delete
xtool2 sdk install /tmp/boop-sdk-extract/darwin.xtoolsdk
find "$HOME/.config/xtool/darwin.xtoolsdk" -name '._*' -delete
rm -rf /tmp/boop-sdk-extract /tmp/darwin.xtoolsdk.tar.gz
bash '{tmp_remote}'
xtool2 sdk status
"#
    );
    let install_cmd = install_cmd.replace(
        &format!("bash '{tmp_remote}'"),
        &format!("{env_prefix}bash '{tmp_remote}'"),
    );
    let inst = provider.exec_stream(ep, &install_cmd, false)?;
    if inst.exit_code != 0 {
        bail!("Failed to install Darwin SDK on worker: {}", inst.output);
    }

    Ok(())
}

/// Build (or reuse cached) slim `darwin-<arch>.xtoolsdk.tar.gz` directly from local `/Applications/Xcode.app`
/// with `COPYFILE_DISABLE=1` so macOS never injects `._*` AppleDouble files.
pub fn ensure_local_darwin_sdk_tarball(arch: &str) -> Result<PathBuf> {
    let sdk_cache = cache_dir()?.join("sdk");
    fs::create_dir_all(&sdk_cache)?;
    let tarball = sdk_cache.join(format!("darwin-{arch}.xtoolsdk.tar.gz"));
    if tarball.exists() {
        return Ok(tarball);
    }

    let mac_xtool = ensure_local_mac_xtool()?;
    let out_dir = sdk_cache.join(arch);
    fs::create_dir_all(&out_dir)?;
    let sdk_bundle = out_dir.join("darwin.xtoolsdk");
    if !sdk_bundle.exists() {
        let xcode_app = detect_local_xcode_app();
        eprintln!(
            "==> Extracting slim Darwin SDK ({arch}) from {}...",
            xcode_app.display()
        );
        let status = Command::new(&mac_xtool)
            .args(["sdk", "build"])
            .arg(&xcode_app)
            .arg(&out_dir)
            .args(["--arch", arch])
            .stdout(std::process::Stdio::from(std::io::stderr()))
            .status()
            .context("Failed to run local `xtool sdk build`")?;
        if !status.success() {
            bail!("Local `xtool sdk build` failed");
        }
    }

    eprintln!("==> Compressing slim Darwin SDK (COPYFILE_DISABLE=1)...");
    let status = Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .args(["-czf"])
        .arg(&tarball)
        .arg("-C")
        .arg(&out_dir)
        .arg("darwin.xtoolsdk")
        .stdout(std::process::Stdio::from(std::io::stderr()))
        .status()
        .context("Failed to compress darwin.xtoolsdk")?;
    if !status.success() {
        bail!("Failed to compress darwin.xtoolsdk");
    }
    Ok(tarball)
}

fn ensure_local_mac_xtool() -> Result<PathBuf> {
    let bin =
        home_dir()?.join(".local/share/boop/xtool-2.0-mac/xtool.app/Contents/Resources/bin/xtool");
    if bin.exists() {
        return Ok(bin);
    }
    let dest = home_dir()?.join(".local/share/boop/xtool-2.0-mac");
    fs::create_dir_all(&dest)?;
    let zip_tmp = std::env::temp_dir().join("boop-xtool-mac.zip");
    eprintln!("==> Downloading macOS xtool 2.0.0-beta.1 for local SDK extraction...");
    let dl = Command::new("curl")
        .args([
            "-fsSL",
            "https://github.com/xtool-org/xtool/releases/download/2.0.0-beta.1/xtool.app.zip",
            "-o",
        ])
        .arg(&zip_tmp)
        .stdout(std::process::Stdio::from(std::io::stderr()))
        .status()?;
    if !dl.success() {
        bail!("Failed to download macOS xtool.app.zip");
    }
    let uz = Command::new("unzip")
        .args(["-oq"])
        .arg(&zip_tmp)
        .arg("-d")
        .arg(&dest)
        .stdout(std::process::Stdio::from(std::io::stderr()))
        .status()?;
    let _ = fs::remove_file(&zip_tmp);
    if !uz.success() || !bin.exists() {
        bail!("Failed to extract macOS xtool CLI binary");
    }
    Ok(bin)
}

/// Detect active Xcode `.app` bundle via `BOOP_XCODE_APP` or `xcode-select -p`, falling back to `/Applications/Xcode.app`.
fn detect_local_xcode_app() -> PathBuf {
    if let Some(p) = std::env::var_os("BOOP_XCODE_APP") {
        return PathBuf::from(p);
    }
    if let Ok(out) = Command::new("xcode-select").arg("-p").output()
        && out.status.success()
    {
        let dev_dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        // e.g. /Applications/Xcode.app/Contents/Developer -> /Applications/Xcode.app
        if let Some(app) = dev_dir.parent().and_then(|p| p.parent())
            && app.extension().and_then(|s| s.to_str()) == Some("app")
            && app.is_dir()
        {
            return app.to_path_buf();
        }
    }
    PathBuf::from("/Applications/Xcode.app")
}

/// Query local macOS `xcrun actool --version` and `xcrun --sdk iphoneos --show-sdk-version`
/// so the remote Linux shims automatically match whatever Xcode version the user has installed.
fn local_toolchain_env_prefix() -> String {
    let mut vars = Vec::new();
    if let Ok(out) = Command::new("xcrun")
        .args(["actool", "--version", "--output-format", "xml1"])
        .output()
        && out.status.success()
        && let Ok(val) = plist::from_bytes::<plist::Value>(&out.stdout)
        && let Some(ver) = val
            .as_dictionary()
            .and_then(|d| d.get("com.apple.actool.version"))
            .and_then(plist::Value::as_dictionary)
    {
        if let Some(b) = ver.get("bundle-version").and_then(plist::Value::as_string) {
            vars.push(format!("BOOP_ACTOOL_BUNDLE_VERSION='{b}'"));
        }
        if let Some(s) = ver
            .get("short-bundle-version")
            .and_then(plist::Value::as_string)
        {
            vars.push(format!("BOOP_ACTOOL_SHORT_VERSION='{s}'"));
        }
    }
    if let Ok(out) = Command::new("xcrun")
        .args(["--sdk", "iphoneos", "--show-sdk-version"])
        .output()
        && out.status.success()
    {
        let sdk_ver = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !sdk_ver.is_empty() {
            vars.push(format!("BOOP_IOS_SDK_VERSION='{sdk_ver}'"));
        }
    }
    if vars.is_empty() {
        String::new()
    } else {
        format!("{} ", vars.join(" "))
    }
}
