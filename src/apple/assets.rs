use crate::cli::BuildTarget;
use crate::config::{AssetsSection, MetalBundleOverlay, cache_dir};
use anyhow::{Context, Result, bail};
use plist::{Dictionary, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Compile configured .metal shader directories locally on macOS (hash-cached, <0.5s cold, <5ms warm)
/// and overlay `default.metallib` (and optional `.metal` sources) into the pulled `.app` bundle.
pub fn apply_metal_overlays(
    project_root: &Path,
    app_bundle: &Path,
    overlays: &[MetalBundleOverlay],
    target: BuildTarget,
) -> Result<usize> {
    if overlays.is_empty() {
        return Ok(0);
    }
    let sdk = match target {
        BuildTarget::Device => "iphoneos",
        BuildTarget::Simulator => "iphonesimulator",
    };

    let mut applied = 0usize;
    for overlay in overlays {
        let src_dir = project_root.join(&overlay.source_dir);
        if !src_dir.is_dir() {
            continue;
        }
        let mut metal_files: Vec<PathBuf> = fs::read_dir(&src_dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("metal"))
            .collect();
        metal_files.sort();
        if metal_files.is_empty() {
            continue;
        }

        let target_bundle_dir = app_bundle.join(&overlay.target_bundle);
        fs::create_dir_all(&target_bundle_dir)?;

        // Compute content hash over SDK + all .metal files
        let mut hasher = Sha256::new();
        hasher.update(sdk.as_bytes());
        for mf in &metal_files {
            hasher.update(
                mf.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .as_bytes(),
            );
            hasher.update(fs::read(mf)?);
        }
        let hash = hex::encode(hasher.finalize())[..16].to_string();

        let cached_dir = cache_dir()?.join("metal").join(&hash);
        let cached_metallib = cached_dir.join("default.metallib");

        if !cached_metallib.exists() {
            fs::create_dir_all(&cached_dir)?;
            let mut air_files = Vec::new();
            for mf in &metal_files {
                let stem = mf.file_stem().and_then(|s| s.to_str()).unwrap_or("shader");
                let air = cached_dir.join(format!("{stem}.air"));
                let out = Command::new("xcrun")
                    .args(["-sdk", sdk, "metal", "-c"])
                    .arg(mf)
                    .arg("-o")
                    .arg(&air)
                    .output()
                    .context("Failed to run `xcrun metal`")?;
                if !out.status.success() {
                    bail!(
                        "Local metal compilation failed for {}: {}",
                        mf.display(),
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
                air_files.push(air);
            }

            let mut link_cmd = Command::new("xcrun");
            link_cmd.args(["-sdk", sdk, "metallib"]);
            for air in &air_files {
                link_cmd.arg(air);
            }
            link_cmd.arg("-o").arg(&cached_metallib);
            let out = link_cmd
                .output()
                .context("Failed to run `xcrun metallib`")?;
            if !out.status.success() {
                bail!(
                    "Local metallib linking failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }

        fs::copy(&cached_metallib, target_bundle_dir.join("default.metallib"))?;
        if overlay.copy_sources {
            for mf in &metal_files {
                if let Some(name) = mf.file_name() {
                    fs::copy(mf, target_bundle_dir.join(name))?;
                }
            }
        }
        applied += 1;
    }

    Ok(applied)
}

/// Compile configured `.xcassets` and/or `.icon` bundles locally on macOS using hash-cached `xcrun actool`,
/// copy the resulting `Assets.car` + app icon PNGs into the pulled `.app`, and recursively merge the
/// generated partial `Info.plist` keys (`CFBundleIcons`, etc.) into `<app_bundle>/Info.plist`.
pub fn apply_asset_overlays(
    project_root: &Path,
    app_bundle: &Path,
    assets: &AssetsSection,
    target: BuildTarget,
) -> Result<bool> {
    if assets.sources.is_empty() {
        return Ok(false);
    }

    let mut resolved_sources = Vec::new();
    for rel in &assets.sources {
        let full = project_root.join(rel);
        if full.exists() {
            resolved_sources.push((rel.clone(), full));
        }
    }
    if resolved_sources.is_empty() {
        return Ok(false);
    }

    let platform = match target {
        BuildTarget::Device => "iphoneos",
        BuildTarget::Simulator => "iphonesimulator",
    };

    let info_plist_path = app_bundle.join("Info.plist");
    let min_os = assets
        .min_deployment_target
        .clone()
        .or_else(|| {
            Value::from_file(&info_plist_path).ok().and_then(|v| {
                v.as_dictionary()
                    .and_then(|d| d.get("MinimumOSVersion"))
                    .and_then(Value::as_string)
                    .map(str::to_string)
            })
        })
        .unwrap_or_else(|| "17.0".to_string());

    // Compute content hash across platform, deployment target, options, and all input files
    let mut hasher = Sha256::new();
    hasher.update(platform.as_bytes());
    hasher.update(b"\0");
    hasher.update(min_os.as_bytes());
    hasher.update(b"\0");
    if let Some(ref icon) = assets.app_icon {
        hasher.update(icon.as_bytes());
    }
    hasher.update([u8::from(assets.include_all_app_icons)]);

    for (rel, full) in &resolved_sources {
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        let mut files = Vec::new();
        collect_asset_files(full, full, &mut files)?;
        files.sort_by(|a, b| a.0.cmp(&b.0));
        for (sub_rel, path) in files {
            hasher.update(sub_rel.as_bytes());
            hasher.update(b"\0");
            hasher.update(fs::read(&path)?);
        }
    }
    let hash = hex::encode(hasher.finalize())[..20].to_string();

    let cache_root = cache_dir()?.join("actool").join(&hash);
    let cache_output = cache_root.join("output");
    let cache_partial = cache_root.join("partial.plist");
    let cache_marker = cache_root.join(".done");

    if !cache_marker.exists() {
        let parent = cache_root.parent().context("missing actool cache parent")?;
        fs::create_dir_all(parent)?;
        let tmp = tempfile::tempdir_in(parent)?;
        let tmp_output = tmp.path().join("output");
        let tmp_partial = tmp.path().join("partial.plist");
        fs::create_dir_all(&tmp_output)?;

        let mut cmd = Command::new("xcrun");
        cmd.args(["actool", "--compile"]).arg(&tmp_output).args([
            "--platform",
            platform,
            "--minimum-deployment-target",
            &min_os,
            "--target-device",
            "iphone",
        ]);
        if let Some(ref icon) = assets.app_icon {
            cmd.args(["--app-icon", icon]);
        }
        if assets.include_all_app_icons {
            cmd.arg("--include-all-app-icons");
        }
        cmd.arg("--output-partial-info-plist").arg(&tmp_partial);
        for (_, full) in &resolved_sources {
            cmd.arg(full);
        }

        let out = cmd
            .output()
            .context("Failed to run `xcrun actool` locally")?;
        if !out.status.success() {
            bail!(
                "Local `xcrun actool` compilation failed:\n{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }

        fs::write(tmp.path().join(".done"), b"ok")?;
        let tmp_path = tmp.keep();
        if fs::rename(&tmp_path, &cache_root).is_err() {
            let _ = fs::remove_dir_all(&tmp_path);
        }
    }

    // Copy compiled Assets.car and icon PNGs into .app bundle
    if cache_output.is_dir() {
        for entry in fs::read_dir(&cache_output)?.flatten() {
            let src = entry.path();
            let dst = app_bundle.join(entry.file_name());
            if src.is_dir() {
                copy_dir_all(&src, &dst)?;
            } else if src.is_file() {
                fs::copy(&src, &dst)?;
            }
        }
    }

    // Recursively merge partial.plist (e.g. CFBundleIcons.CFBundlePrimaryIcon.CFBundleIconFiles)
    if cache_partial.is_file() && info_plist_path.is_file() {
        let partial_val = Value::from_file(&cache_partial)
            .with_context(|| format!("Failed to read {}", cache_partial.display()))?;
        let mut info_val = Value::from_file(&info_plist_path)
            .with_context(|| format!("Failed to read {}", info_plist_path.display()))?;
        if let (Some(dst), Some(src)) = (info_val.as_dictionary_mut(), partial_val.as_dictionary())
        {
            merge_plist_dict(dst, src);
            info_val
                .to_file_xml(&info_plist_path)
                .with_context(|| format!("Failed to write {}", info_plist_path.display()))?;
        }
    }

    Ok(true)
}

fn collect_asset_files(root: &Path, cur: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    if cur.is_file() {
        let rel = cur
            .strip_prefix(root)
            .unwrap_or(cur)
            .to_string_lossy()
            .to_string();
        out.push((rel, cur.to_path_buf()));
        return Ok(());
    }
    for entry in fs::read_dir(cur)?.flatten() {
        let p = entry.path();
        if p.file_name().and_then(|s| s.to_str()) == Some(".DS_Store") {
            continue;
        }
        if p.is_dir() {
            collect_asset_files(root, &p, out)?;
        } else if p.is_file() {
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .to_string();
            out.push((rel, p));
        }
    }
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)?.flatten() {
        let sp = entry.path();
        let dp = dst.join(entry.file_name());
        if sp.is_dir() {
            copy_dir_all(&sp, &dp)?;
        } else if sp.is_file() {
            fs::copy(&sp, &dp)?;
        }
    }
    Ok(())
}

/// Recursively merge `src` plist dictionary into `dst`, preserving existing sibling keys
/// (such as `CFBundleAlternateIcons`) while adding/updating keys emitted by `actool`.
fn merge_plist_dict(dst: &mut Dictionary, src: &Dictionary) {
    for (k, v) in src {
        match (dst.get_mut(k), v) {
            (Some(Value::Dictionary(dst_sub)), Value::Dictionary(src_sub)) => {
                merge_plist_dict(dst_sub, src_sub);
            }
            _ => {
                dst.insert(k.clone(), v.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_actool_partial_icons_while_preserving_alternate_icons() {
        let mut primary_existing = Dictionary::new();
        primary_existing.insert("CFBundleIconName".into(), Value::String("AppIcon".into()));
        let mut alt = Dictionary::new();
        alt.insert(
            "AppIcon-Cobalt".into(),
            Value::Dictionary(Dictionary::new()),
        );
        let mut icons_existing = Dictionary::new();
        icons_existing.insert(
            "CFBundlePrimaryIcon".into(),
            Value::Dictionary(primary_existing),
        );
        icons_existing.insert("CFBundleAlternateIcons".into(), Value::Dictionary(alt));
        let mut dst = Dictionary::new();
        dst.insert("CFBundleIcons".into(), Value::Dictionary(icons_existing));

        let mut primary_partial = Dictionary::new();
        primary_partial.insert(
            "CFBundleIconFiles".into(),
            Value::Array(vec![Value::String("AppIcon60x60".into())]),
        );
        primary_partial.insert("CFBundleIconName".into(), Value::String("AppIcon".into()));
        let mut icons_partial = Dictionary::new();
        icons_partial.insert(
            "CFBundlePrimaryIcon".into(),
            Value::Dictionary(primary_partial),
        );
        let mut src = Dictionary::new();
        src.insert("CFBundleIcons".into(), Value::Dictionary(icons_partial));

        merge_plist_dict(&mut dst, &src);

        let merged_icons = dst.get("CFBundleIcons").unwrap().as_dictionary().unwrap();
        assert!(merged_icons.contains_key("CFBundleAlternateIcons"));
        let primary = merged_icons
            .get("CFBundlePrimaryIcon")
            .unwrap()
            .as_dictionary()
            .unwrap();
        assert_eq!(
            primary.get("CFBundleIconName").and_then(Value::as_string),
            Some("AppIcon")
        );
        assert_eq!(
            primary
                .get("CFBundleIconFiles")
                .and_then(Value::as_array)
                .map(|a| a.len()),
            Some(1)
        );
    }
}
