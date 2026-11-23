use anyhow::{Context, Result, bail, ensure};
use chrono::{Timelike, Utc};
use plist::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

/// Stamp a unique sortable `CFBundleVersion` into the pulled `.app/Info.plist` (without touching Git)
/// so every published agent build gets a distinct entry in Firebase App Distribution.
/// Returns `(CFBundleShortVersionString, CFBundleVersion)`.
pub fn stamp_build_version(
    app_bundle: &Path,
    explicit_version: Option<&str>,
) -> Result<(String, String)> {
    let info_plist = app_bundle.join("Info.plist");
    ensure!(
        info_plist.is_file(),
        "Cannot stamp build version: missing {}",
        info_plist.display()
    );

    let mut val = Value::from_file(&info_plist)
        .with_context(|| format!("Failed to read {}", info_plist.display()))?;
    let dict = val
        .as_dictionary_mut()
        .context("Info.plist root is not a dictionary")?;

    let display_version = dict
        .get("CFBundleShortVersionString")
        .and_then(Value::as_string)
        .filter(|s| !s.is_empty())
        .unwrap_or("1.0")
        .to_string();

    let build_version = match explicit_version {
        Some(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => generate_build_version(),
    };

    dict.insert(
        "CFBundleVersion".to_string(),
        Value::String(build_version.clone()),
    );

    val.to_file_xml(&info_plist)
        .with_context(|| format!("Failed to write {}", info_plist.display()))?;

    Ok((display_version, build_version))
}

/// Generates a monotonically increasing, Apple-valid two-part numeric `CFBundleVersion`
/// (`YYYYMMDD.<HHMMSS without leading zeros>`, both valid 32-bit positive integers).
pub fn generate_build_version() -> String {
    let now = Utc::now();
    let time_num = now.hour() * 10_000 + now.minute() * 100 + now.second();
    format!("{}.{}", now.format("%Y%m%d"), time_num)
}

/// Package a signed `.app` bundle into a clean `.ipa` (`Payload/<AppName>.app/...`)
/// using macOS `ditto --norsrc` so permissions, symlinks, and `_CodeSignature` stay intact
/// without macOS resource-fork (`__MACOSX` / `._*`) artifacts.
/// Returns the resulting `.ipa` file size in bytes.
pub fn package_ipa(app_bundle: &Path, output_ipa: &Path) -> Result<u64> {
    ensure!(
        app_bundle.is_dir(),
        "App bundle not found: {}",
        app_bundle.display()
    );
    let app_name = app_bundle
        .file_name()
        .and_then(|s| s.to_str())
        .context("Invalid .app bundle name")?;
    ensure!(
        app_name.ends_with(".app"),
        "Expected a .app bundle, got {}",
        app_bundle.display()
    );

    if let Some(parent) = output_ipa.parent() {
        fs::create_dir_all(parent)?;
    }
    if output_ipa.exists() {
        fs::remove_file(output_ipa)?;
    }

    let staging = tempfile::tempdir().context("Failed to create IPA staging directory")?;
    let payload_dir = staging.path().join("Payload");
    fs::create_dir_all(&payload_dir)?;
    let staged_app = payload_dir.join(app_name);

    let copy_out = Command::new("/usr/bin/ditto")
        .arg("--norsrc")
        .arg(app_bundle)
        .arg(&staged_app)
        .output()
        .context("Failed to run `/usr/bin/ditto` while staging Payload")?;
    if !copy_out.status.success() {
        bail!(
            "Failed to stage {} into Payload: {}",
            app_bundle.display(),
            String::from_utf8_lossy(&copy_out.stderr)
        );
    }

    let zip_out = Command::new("/usr/bin/ditto")
        .args(["-c", "-k", "--norsrc", "--keepParent"])
        .arg(&payload_dir)
        .arg(output_ipa)
        .output()
        .context("Failed to run `/usr/bin/ditto` while creating .ipa")?;
    if !zip_out.status.success() {
        bail!(
            "Failed to package IPA {}: {}",
            output_ipa.display(),
            String::from_utf8_lossy(&zip_out.stderr)
        );
    }

    let size = fs::metadata(output_ipa)
        .with_context(|| format!("Failed to stat packaged IPA {}", output_ipa.display()))?
        .len();
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plist::Dictionary;
    use tempfile::tempdir;

    #[test]
    fn stamps_build_version_without_losing_keys() {
        let dir = tempdir().unwrap();
        let app = dir.path().join("Demo.app");
        fs::create_dir_all(&app).unwrap();
        let mut dict = Dictionary::new();
        dict.insert(
            "CFBundleIdentifier".into(),
            Value::String("com.test.demo".into()),
        );
        dict.insert(
            "CFBundleShortVersionString".into(),
            Value::String("2.4".into()),
        );
        dict.insert("CFBundleVersion".into(), Value::String("1".into()));
        Value::Dictionary(dict)
            .to_file_xml(app.join("Info.plist"))
            .unwrap();

        let (short_v, build_v) = stamp_build_version(&app, Some("20261123.83000")).unwrap();
        assert_eq!(short_v, "2.4");
        assert_eq!(build_v, "20261123.83000");

        let reloaded = Value::from_file(app.join("Info.plist")).unwrap();
        let d = reloaded.as_dictionary().unwrap();
        assert_eq!(
            d.get("CFBundleVersion").and_then(Value::as_string),
            Some("20261123.83000")
        );
        assert_eq!(
            d.get("CFBundleIdentifier").and_then(Value::as_string),
            Some("com.test.demo")
        );
    }

    #[test]
    fn generated_build_version_is_valid_numeric_dotted_pair() {
        let v = generate_build_version();
        let parts: Vec<&str> = v.split('.').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].len(), 8);
        assert!(parts[0].parse::<u32>().is_ok());
        assert!(parts[1].parse::<u32>().is_ok());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn packages_clean_payload_ipa_without_appledouble() {
        let dir = tempdir().unwrap();
        let app = dir.path().join("Demo.app");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("Info.plist"), "<plist/>").unwrap();
        fs::write(app.join("Demo"), b"binary").unwrap();

        let ipa = dir.path().join("out/Demo.ipa");
        let size = package_ipa(&app, &ipa).unwrap();
        assert!(size > 0);
        assert!(ipa.is_file());

        let list = Command::new("/usr/bin/unzip")
            .args(["-Z1"])
            .arg(&ipa)
            .output()
            .unwrap();
        assert!(list.status.success());
        let entries: Vec<String> = String::from_utf8_lossy(&list.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        assert!(entries.iter().any(|e| e == "Payload/Demo.app/Info.plist"));
        assert!(entries.iter().any(|e| e == "Payload/Demo.app/Demo"));
        assert!(
            !entries
                .iter()
                .any(|e| e.contains("__MACOSX") || e.contains("/._"))
        );
    }
}
