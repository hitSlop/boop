use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

pub struct FirebasePublishRequest<'a> {
    pub ipa_path: &'a Path,
    pub app_id: &'a str,
    pub release_notes: &'a str,
    pub groups: &'a [String],
    pub testers: &'a [String],
    pub stream: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedDistributeOutput {
    pub upload_result: Option<String>,
    pub display_version: Option<String>,
    pub build_version: Option<String>,
    pub firebase_console_uri: Option<String>,
    pub testing_uri: Option<String>,
    pub binary_download_uri: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FirebasePublishResult {
    pub parsed: ParsedDistributeOutput,
    pub raw_output: String,
    pub duration_ms: u64,
}

/// Format rich agent-friendly release notes for Firebase App Distribution.
pub fn format_release_notes(
    name: &str,
    notes: Option<&str>,
    branch: Option<&str>,
    commit: Option<&str>,
    dirty: bool,
    worktree_id: &str,
    build_id: &str,
) -> String {
    let mut out = String::new();
    out.push_str(name.trim());
    if let Some(n) = notes.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("\n\n");
        out.push_str(n);
    }

    let mut meta = Vec::new();
    if let Some(b) = branch.filter(|s| !s.is_empty()) {
        if let Some(c) = commit.filter(|s| !s.is_empty()) {
            let suffix = if dirty { "+dirty" } else { "" };
            meta.push(format!("Branch: {b} ({c}{suffix})"));
        } else {
            meta.push(format!("Branch: {b}"));
        }
    } else if let Some(c) = commit.filter(|s| !s.is_empty()) {
        let suffix = if dirty { "+dirty" } else { "" };
        meta.push(format!("Commit: {c}{suffix}"));
    }
    meta.push(format!("Worktree: {worktree_id}"));
    meta.push(format!("Build: {build_id}"));

    out.push_str("\n\n---\n");
    out.push_str(&meta.join(" · "));
    out
}

/// Upload a signed `.ipa` to Firebase App Distribution using the locally authenticated `firebase` CLI.
pub fn upload_firebase(req: &FirebasePublishRequest<'_>) -> Result<FirebasePublishResult> {
    let started = Instant::now();

    let mut notes_file = tempfile::NamedTempFile::new()
        .context("Failed to create temp file for Firebase release notes")?;
    notes_file.write_all(req.release_notes.as_bytes())?;
    notes_file.flush()?;

    let mut cmd = Command::new("firebase");
    cmd.args(["appdistribution:distribute"])
        .arg(req.ipa_path)
        .args(["--app", req.app_id, "--non-interactive"])
        .arg("--release-notes-file")
        .arg(notes_file.path());

    if !req.groups.is_empty() {
        cmd.args(["--groups", &req.groups.join(",")]);
    }
    if !req.testers.is_empty() {
        cmd.args(["--testers", &req.testers.join(",")]);
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "`firebase` CLI not found in PATH. Install with `npm install -g firebase-tools` \
                 (or `brew install firebase-cli`) and run `firebase login`."
            )
        } else {
            anyhow::anyhow!("Failed to spawn `firebase appdistribution:distribute`: {e}")
        }
    })?;

    let stdout = child.stdout.take().context("missing stdout pipe")?;
    let stderr = child.stderr.take().context("missing stderr pipe")?;
    let stream = req.stream;

    let err_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if stream {
                eprintln!("{line}");
            }
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });

    let mut out_buf = String::new();
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if stream {
            println!("{line}");
        }
        out_buf.push_str(&line);
        out_buf.push('\n');
    }

    let err_buf = err_handle.join().unwrap_or_default();
    let status = child.wait()?;
    let combined = if err_buf.is_empty() {
        out_buf.clone()
    } else {
        format!("{out_buf}\n{err_buf}")
    };

    if !status.success() {
        let lower = combined.to_lowercase();
        if lower.contains("failed to authenticate") || lower.contains("not logged in") {
            bail!(
                "Firebase CLI is not authenticated. Run `firebase login` first.\n{}",
                combined.trim()
            );
        }
        if lower.contains("press") && lower.contains("get started") {
            bail!(
                "Firebase App Distribution is not onboarded for app {}. Open the Firebase console \
                 App Distribution page and click 'Get started'.\n{}",
                req.app_id,
                combined.trim()
            );
        }
        bail!(
            "`firebase appdistribution:distribute` failed (exit {:?}):\n{}",
            status.code(),
            combined.trim()
        );
    }

    let parsed = parse_distribute_output(&combined);
    Ok(FirebasePublishResult {
        parsed,
        raw_output: combined,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

/// Parse Firebase App Distribution CLI stdout lines (from `appdistribution-distribute.js`).
pub fn parse_distribute_output(output: &str) -> ParsedDistributeOutput {
    let mut parsed = ParsedDistributeOutput::default();

    for raw_line in output.lines() {
        let line = strip_ansi(raw_line).trim().to_string();
        if let Some(url) = extract_url_after(&line, "View this release in the Firebase console:") {
            parsed.firebase_console_uri = Some(url);
        } else if let Some(url) =
            extract_url_after(&line, "Share this release with testers who have access:")
        {
            parsed.testing_uri = Some(url);
        } else if let Some(url) = extract_url_after(
            &line,
            "Download the release binary (link expires in 1 hour):",
        ) {
            parsed.binary_download_uri = Some(url);
        }

        if parsed.upload_result.is_none() {
            if let Some(rest) = line.split_once("uploaded new release ").map(|(_, r)| r) {
                parsed.upload_result = Some("created".into());
                parse_version_tuple(rest, &mut parsed);
            } else if let Some(rest) = line
                .split_once("uploaded update to existing release ")
                .map(|(_, r)| r)
            {
                parsed.upload_result = Some("updated".into());
                parse_version_tuple(rest, &mut parsed);
            } else if let Some(rest) = line
                .split_once("re-uploaded already existing release ")
                .map(|(_, r)| r)
            {
                parsed.upload_result = Some("unmodified".into());
                parse_version_tuple(rest, &mut parsed);
            } else if let Some(rest) = line.split_once("uploaded release ").map(|(_, r)| r) {
                parsed.upload_result = Some("uploaded".into());
                parse_version_tuple(rest, &mut parsed);
            }
        }
    }

    // Fallback scan for testingUri if Firebase CLI changes prefix wording slightly
    if parsed.testing_uri.is_none() {
        for raw_line in output.lines() {
            let clean = strip_ansi(raw_line);
            if let Some(idx) = clean.find("https://appdistribution.firebase.google.com/testerapps/")
            {
                let url = clean[idx..]
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string();
                if !url.is_empty() {
                    parsed.testing_uri = Some(url);
                    break;
                }
            }
        }
    }

    parsed
}

fn extract_url_after(line: &str, marker: &str) -> Option<String> {
    let (_, rest) = line.split_once(marker)?;
    let url = rest.split_whitespace().next()?;
    if url.starts_with("http://") || url.starts_with("https://") {
        Some(url.to_string())
    } else {
        None
    }
}

fn parse_version_tuple(rest: &str, out: &mut ParsedDistributeOutput) {
    // Format: `<displayVersion> (<buildVersion>) successfully!`
    let trimmed = rest.trim();
    if let Some((disp, after_open)) = trimmed.split_once(" (")
        && let Some((build, _)) = after_open.split_once(')')
    {
        if !disp.trim().is_empty() {
            out.display_version = Some(disp.trim().to_string());
        }
        if !build.trim().is_empty() {
            out.build_version = Some(build.trim().to_string());
        }
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for nc in chars.by_ref() {
                if nc.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Read-only check for `boop doctor` verifying `firebase` CLI installation & auth.
pub fn check_firebase_cli() -> (bool, String) {
    let ver = Command::new("firebase").arg("--version").output();
    let Ok(ver_out) = ver else {
        return (
            false,
            "firebase CLI not found (install `firebase-tools` for `boop publish`)".into(),
        );
    };
    if !ver_out.status.success() {
        return (false, "`firebase --version` failed".into());
    }
    let version = String::from_utf8_lossy(&ver_out.stdout).trim().to_string();

    let login = Command::new("firebase").arg("login:list").output();
    let account = login.ok().filter(|o| o.status.success()).and_then(|o| {
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .find(|l| l.contains("Logged in as "))
            .map(|l| l.trim().to_string())
    });

    match account {
        Some(acc) => (true, format!("firebase {version} ({acc})")),
        None => (
            false,
            format!("firebase {version} installed, but not logged in (run `firebase login`)"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_firebase_cli_distribute_output() {
        let sample = "\x1b[36mi\x1b[39m  uploading binary...\n\
            \x1b[32m✔\x1b[39m  uploaded new release 1.0 (20261123.83000) successfully!\n\
            \x1b[32m✔\x1b[39m  View this release in the Firebase console: https://console.firebase.google.com/project/demo-app/appdistribution/app/ios:com.example.demo/releases/abc123\n\
            \x1b[32m✔\x1b[39m  Share this release with testers who have access: https://appdistribution.firebase.google.com/testerapps/1:000000000000:ios:abcdef0123456789/releases/abc123\n\
            \x1b[32m✔\x1b[39m  Download the release binary (link expires in 1 hour): https://firebaseappdistribution.googleapis.com/download/xyz\n";

        let p = parse_distribute_output(sample);
        assert_eq!(p.upload_result.as_deref(), Some("created"));
        assert_eq!(p.display_version.as_deref(), Some("1.0"));
        assert_eq!(p.build_version.as_deref(), Some("20261123.83000"));
        assert_eq!(
            p.firebase_console_uri.as_deref(),
            Some(
                "https://console.firebase.google.com/project/demo-app/appdistribution/app/ios:com.example.demo/releases/abc123"
            )
        );
        assert_eq!(
            p.testing_uri.as_deref(),
            Some(
                "https://appdistribution.firebase.google.com/testerapps/1:000000000000:ios:abcdef0123456789/releases/abc123"
            )
        );
        assert_eq!(
            p.binary_download_uri.as_deref(),
            Some("https://firebaseappdistribution.googleapis.com/download/xyz")
        );
    }

    #[test]
    fn formats_release_notes_with_git_metadata() {
        let notes = format_release_notes(
            "New keyboard animation",
            Some("Spring transition and haptic tuning"),
            Some("feat/keyboard-spring"),
            Some("a1b2c3d"),
            true,
            "wt-1234",
            "bld-001",
        );
        assert!(notes.starts_with("New keyboard animation\n\nSpring transition and haptic tuning"));
        assert!(notes.contains("Branch: feat/keyboard-spring (a1b2c3d+dirty)"));
        assert!(notes.contains("Worktree: wt-1234 · Build: bld-001"));
    }
}
