use crate::config::{cache_dir, home_dir};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use plist::{Dictionary, Value};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SigningProfileKind {
    Development,
    AdHoc,
    Auto,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProfileType {
    Development,
    AdHoc,
    Enterprise,
    AppStore,
}

impl ProfileType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProfileType::Development => "development",
            ProfileType::AdHoc => "adhoc",
            ProfileType::Enterprise => "enterprise",
            ProfileType::AppStore => "appstore",
        }
    }
}

pub fn classify_profile(
    get_task_allow: bool,
    provisions_all_devices: bool,
    device_count: usize,
) -> ProfileType {
    if get_task_allow {
        ProfileType::Development
    } else if provisions_all_devices {
        ProfileType::Enterprise
    } else if device_count > 0 {
        ProfileType::AdHoc
    } else {
        ProfileType::AppStore
    }
}

#[derive(Debug, Clone)]
pub struct MatchedProfile {
    pub path: PathBuf,
    pub name: String,
    pub kind: ProfileType,
    pub cert_sha1: String,
    pub entitlements: Dictionary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedProfileMeta {
    mtime_secs: u64,
    size: u64,
    name: String,
    teams: Vec<String>,
    app_id: String,
    get_task_allow: bool,
    #[serde(default)]
    provisions_all_devices: bool,
    expiration_rfc3339: String,
    provisioned_devices: Vec<String>,
    cert_sha1s: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProfileIndexCache {
    entries: BTreeMap<String, CachedProfileMeta>,
}

fn profile_cache_path() -> Result<PathBuf> {
    Ok(cache_dir()?.join("profiles-cms-cache-v2.json"))
}

fn load_profile_cache() -> ProfileIndexCache {
    profile_cache_path()
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_profile_cache(cache: &ProfileIndexCache) {
    if let Ok(path) = profile_cache_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(cache) {
            let _ = fs::write(path, json);
        }
    }
}

/// Sign an `.app` bundle locally on macOS using Keychain identity + CMS-verified provisioning profile.
/// Uses a CMS-decoded profile metadata cache + content-addressed signed framework cache + parallel signing
/// so unchanged third-party frameworks (e.g. Firebase/Google) aren't re-signed every edit.
pub fn sign_for_device(
    app_bundle: &Path,
    bundle_id: &str,
    team_id: &str,
    target_udid: Option<&str>,
    explicit_identity: Option<&str>,
    explicit_profile: Option<&str>,
) -> Result<MatchedProfile> {
    sign_bundle_with_kind(
        app_bundle,
        bundle_id,
        team_id,
        target_udid,
        explicit_identity,
        explicit_profile,
        SigningProfileKind::Development,
    )
}

/// Sign an `.app` bundle locally on macOS for OTA distribution (Ad Hoc preferred, optional Development fallback).
pub fn sign_for_distribution(
    app_bundle: &Path,
    bundle_id: &str,
    team_id: &str,
    target_udid: Option<&str>,
    explicit_identity: Option<&str>,
    explicit_profile: Option<&str>,
    kind: SigningProfileKind,
) -> Result<MatchedProfile> {
    sign_bundle_with_kind(
        app_bundle,
        bundle_id,
        team_id,
        target_udid,
        explicit_identity,
        explicit_profile,
        kind,
    )
}

fn sign_bundle_with_kind(
    app_bundle: &Path,
    bundle_id: &str,
    team_id: &str,
    target_udid: Option<&str>,
    explicit_identity: Option<&str>,
    explicit_profile: Option<&str>,
    kind: SigningProfileKind,
) -> Result<MatchedProfile> {
    let installed_ids = list_codesign_identities()?;
    let matched = if let Some(prof_path) = explicit_profile {
        cms_decode_and_match_profile(
            Path::new(prof_path),
            bundle_id,
            team_id,
            target_udid,
            &installed_ids,
            explicit_identity,
        )?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Explicit provisioning profile {prof_path} does not match {team_id}.{bundle_id}"
            )
        })?
    } else {
        find_best_profile_with_kind(
            bundle_id,
            team_id,
            target_udid,
            &installed_ids,
            explicit_identity,
            kind,
        )?
    };

    // 1. Embed provisioning profile
    fs::copy(&matched.path, app_bundle.join("embedded.mobileprovision"))
        .context("Failed to copy embedded.mobileprovision into .app")?;

    // 2. Write merged entitlements plist to a temporary file
    let ent_path = std::env::temp_dir().join(format!("boop-ent-{}.plist", uuid::Uuid::new_v4()));
    let mut ent = matched.entitlements.clone();
    ent.insert(
        "application-identifier".to_string(),
        Value::String(format!("{team_id}.{bundle_id}")),
    );
    plist::to_file_xml(&ent_path, &Value::Dictionary(ent))
        .context("Failed to write entitlements plist")?;

    // 3. Sign embedded frameworks & dylibs inside-out in parallel, reusing cached signatures
    // for unchanged third-party frameworks (keyed by binary+Info.plist SHA256 + cert SHA1).
    let frameworks_dir = app_bundle.join("Frameworks");
    if frameworks_dir.is_dir() {
        let mut items: Vec<PathBuf> = fs::read_dir(&frameworks_dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|s| s.to_str()),
                    Some("framework") | Some("dylib")
                )
            })
            .collect();
        items.sort();

        let cert = matched.cert_sha1.as_str();
        std::thread::scope(|s| -> Result<()> {
            let mut handles = Vec::with_capacity(items.len());
            for item in &items {
                handles.push(s.spawn(move || sign_embedded_item_cached(item, cert)));
            }
            for h in handles {
                h.join()
                    .map_err(|_| anyhow::anyhow!("Framework signing thread panicked"))??;
            }
            Ok(())
        })?;
    }

    // 4. Sign main .app bundle with entitlements
    let sign_res = run_codesign(&matched.cert_sha1, Some(&ent_path), app_bundle);
    let _ = fs::remove_file(&ent_path);
    sign_res?;

    // 5. Fast strict verification (~0.2s)
    let verify = Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app_bundle)
        .output()
        .context("Failed to run `codesign --verify`")?;
    if !verify.status.success() {
        bail!(
            "codesign verification failed: {}",
            String::from_utf8_lossy(&verify.stderr)
        );
    }

    Ok(matched)
}

/// Content-addressed signature cache for embedded frameworks.
/// Unchanged small/third-party frameworks (FirebaseAnalytics, GoogleAppMeasurement, etc.)
/// restore their signed `_CodeSignature` + signed binary in ~2ms instead of ~1.2s each.
fn sign_embedded_item_cached(item: &Path, cert_sha1: &str) -> Result<()> {
    // Only cache directory .framework bundles under 10 MB (avoids duplicating huge app frameworks)
    if item.is_dir()
        && item.extension().and_then(|s| s.to_str()) == Some("framework")
        && let Ok(Some(cache_key)) = compute_small_framework_key(item, cert_sha1)
        && let Ok(cdir) = cache_dir()
    {
        let cached_fw = cdir.join("signed-frameworks").join(&cache_key);
        if cached_fw.is_dir() && cached_fw.join("_CodeSignature").is_dir() {
            // Restore cached signed framework cleanly
            let _ = fs::remove_dir_all(item);
            copy_dir_recursive(&cached_fw, item)?;
            return Ok(());
        }
        // Cache miss: sign in place and save into cache
        run_codesign(cert_sha1, None, item)?;
        let tmp_cache = cdir
            .join("signed-frameworks")
            .join(format!("{cache_key}.tmp-{}", uuid::Uuid::new_v4()));
        let _ = fs::create_dir_all(tmp_cache.parent().unwrap());
        if copy_dir_recursive(item, &tmp_cache).is_ok()
            && fs::rename(&tmp_cache, &cached_fw).is_err()
        {
            let _ = fs::remove_dir_all(&tmp_cache);
        }
        return Ok(());
    }

    run_codesign(cert_sha1, None, item)
}

fn compute_small_framework_key(fw_dir: &Path, cert_sha1: &str) -> Result<Option<String>> {
    const MAX_CACHEABLE_BYTES: u64 = 10 * 1024 * 1024;
    let mut files = Vec::new();
    let mut total_bytes = 0u64;
    collect_files_excluding_codesig(fw_dir, fw_dir, &mut files, &mut total_bytes)?;
    if total_bytes > MAX_CACHEABLE_BYTES {
        return Ok(None);
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    hasher.update(cert_sha1.as_bytes());
    for (rel, full) in files {
        hasher.update(rel.as_bytes());
        hasher.update(fs::read(&full)?);
    }
    Ok(Some(hex::encode(hasher.finalize())[..24].to_string()))
}

fn collect_files_excluding_codesig(
    root: &Path,
    cur: &Path,
    out: &mut Vec<(String, PathBuf)>,
    total: &mut u64,
) -> Result<()> {
    for entry in fs::read_dir(cur)?.flatten() {
        let p = entry.path();
        if p.file_name().and_then(|s| s.to_str()) == Some("_CodeSignature") {
            continue;
        }
        if p.is_dir() {
            collect_files_excluding_codesig(root, &p, out, total)?;
        } else if p.is_file() {
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .to_string();
            *total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push((rel, p));
        }
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)?.flatten() {
        let sp = entry.path();
        let dp = dst.join(entry.file_name());
        if sp.is_dir() {
            copy_dir_recursive(&sp, &dp)?;
        } else {
            fs::copy(&sp, &dp)?;
        }
    }
    Ok(())
}

/// Fast ad-hoc signing for local iOS Simulator builds.
pub fn sign_for_simulator(app_bundle: &Path) -> Result<()> {
    let frameworks_dir = app_bundle.join("Frameworks");
    if frameworks_dir.is_dir() {
        let items: Vec<PathBuf> = fs::read_dir(&frameworks_dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|s| s.to_str()),
                    Some("framework") | Some("dylib")
                )
            })
            .collect();
        std::thread::scope(|s| -> Result<()> {
            let mut handles = Vec::new();
            for item in &items {
                handles.push(s.spawn(move || run_codesign("-", None, item)));
            }
            for h in handles {
                h.join()
                    .map_err(|_| anyhow::anyhow!("Sim framework signing panicked"))??;
            }
            Ok(())
        })?;
    }
    run_codesign("-", None, app_bundle)
}

fn run_codesign(identity: &str, entitlements: Option<&Path>, target: &Path) -> Result<()> {
    let mut cmd = Command::new("codesign");
    cmd.args(["--force", "--sign", identity, "--timestamp=none"]);
    if let Some(ent) = entitlements {
        cmd.arg("--entitlements").arg(ent);
    }
    cmd.arg(target);
    let out = cmd
        .output()
        .with_context(|| format!("Failed to spawn codesign on {}", target.display()))?;
    if !out.status.success() {
        bail!(
            "codesign failed on {}: {}",
            target.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

pub fn list_codesign_identities() -> Result<Vec<(String, String)>> {
    let out = Command::new("security")
        .args(["find-identity", "-v", "-p", "codesigning"])
        .output()
        .context("Failed to run `security find-identity`")?;
    if !out.status.success() {
        bail!("`security find-identity` failed");
    }
    Ok(parse_codesign_identities(&String::from_utf8_lossy(
        &out.stdout,
    )))
}

pub fn parse_codesign_identities(stdout: &str) -> Vec<(String, String)> {
    let mut ids = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() >= 3
            && parts[1].len() == 40
            && parts[1].chars().all(|c| c.is_ascii_hexdigit())
        {
            let sha1 = parts[1].to_uppercase();
            let label = trimmed
                .split_once('"')
                .and_then(|(_, rest)| rest.rsplit_once('"'))
                .map(|(name, _)| name.to_string())
                .unwrap_or_default();
            ids.push((sha1, label));
        }
    }
    ids
}

fn provisioning_profile_dirs() -> Result<Vec<PathBuf>> {
    let home = home_dir()?;
    Ok(vec![
        home.join("Library/Developer/Xcode/UserData/Provisioning Profiles"),
        home.join("Library/MobileDevice/Provisioning Profiles"),
    ])
}

/// Finds the best matching non-expired development provisioning profile using a persistent
/// CMS-decoded metadata cache keyed by `(path, mtime, size)`, then CMS-verifies the chosen
/// single profile with `security cms -D -i` before returning its entitlements.
pub fn find_best_profile(
    bundle_id: &str,
    team_id: &str,
    target_udid: Option<&str>,
    installed_ids: &[(String, String)],
    explicit_identity: Option<&str>,
) -> Result<MatchedProfile> {
    find_best_profile_with_kind(
        bundle_id,
        team_id,
        target_udid,
        installed_ids,
        explicit_identity,
        SigningProfileKind::Development,
    )
}

/// Finds the best matching non-expired provisioning profile for the requested `SigningProfileKind`.
/// For `SigningProfileKind::Auto`, prefers Ad Hoc (or Enterprise) and falls back to Development.
pub fn find_best_profile_with_kind(
    bundle_id: &str,
    team_id: &str,
    target_udid: Option<&str>,
    installed_ids: &[(String, String)],
    explicit_identity: Option<&str>,
    kind: SigningProfileKind,
) -> Result<MatchedProfile> {
    let mut cache = load_profile_cache();
    let mut cache_dirty = false;

    let exact = format!("{team_id}.{bundle_id}");
    let wildcard = format!("{team_id}.*");
    let now = Utc::now();

    let mut best: Option<(u8, String, PathBuf, String)> = None;

    for dir in provisioning_profile_dirs()? {
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&dir)?.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("mobileprovision") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let mtime_secs = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let size = meta.len();
            let key = p.to_string_lossy().to_string();

            let cached = match cache.entries.get(&key) {
                Some(c) if c.mtime_secs == mtime_secs && c.size == size => c.clone(),
                _ => {
                    let Some(decoded) = decode_profile_meta_via_cms(&p, mtime_secs, size) else {
                        continue;
                    };
                    cache.entries.insert(key.clone(), decoded.clone());
                    cache_dirty = true;
                    decoded
                }
            };

            if let Ok(dt) = DateTime::parse_from_rfc3339(&cached.expiration_rfc3339)
                && dt.with_timezone(&Utc) < now
            {
                continue;
            }
            if !cached.teams.iter().any(|t| t == team_id) {
                continue;
            }

            let ptype = classify_profile(
                cached.get_task_allow,
                cached.provisions_all_devices,
                cached.provisioned_devices.len(),
            );
            let kind_bonus: u8 = match (kind, ptype) {
                (SigningProfileKind::Development, ProfileType::Development) => 0,
                (SigningProfileKind::AdHoc, ProfileType::AdHoc | ProfileType::Enterprise) => 0,
                (SigningProfileKind::Auto, ProfileType::AdHoc | ProfileType::Enterprise) => 16,
                (SigningProfileKind::Auto, ProfileType::Development) => 0,
                _ => continue,
            };

            let mut score: u8 = kind_bonus
                + if cached.app_id == exact {
                    4
                } else if cached.app_id == wildcard {
                    2
                } else {
                    continue;
                };
            if let Some(udid) = target_udid
                && cached.provisioned_devices.iter().any(|d| d == udid)
            {
                score += 1;
            }
            let chosen_sha1 = if let Some(exp_id) = explicit_identity {
                exp_id.to_uppercase()
            } else if let Some((sha1, _)) = installed_ids
                .iter()
                .find(|(sha1, _)| cached.cert_sha1s.iter().any(|pc| pc == sha1))
            {
                sha1.clone()
            } else {
                continue;
            };

            let replace = match &best {
                None => true,
                Some((bs, be, _, _)) => (score, &cached.expiration_rfc3339) > (*bs, be),
            };
            if replace {
                best = Some((score, cached.expiration_rfc3339, p, chosen_sha1));
            }
        }
    }

    if cache_dirty {
        save_profile_cache(&cache);
    }

    let (_, _, best_path, _) = best.ok_or_else(|| {
        let label = match kind {
            SigningProfileKind::Development => "development",
            SigningProfileKind::AdHoc => "Ad Hoc distribution",
            SigningProfileKind::Auto => "Ad Hoc or development",
        };
        anyhow::anyhow!(
            "No matching non-expired {label} provisioning profile found for {team_id}.{bundle_id} \
             with a local Keychain signing identity."
        )
    })?;

    // Final CMS-verified decode of the single selected profile for live entitlements
    cms_decode_and_match_profile(
        &best_path,
        bundle_id,
        team_id,
        target_udid,
        installed_ids,
        explicit_identity,
    )?
    .ok_or_else(|| {
        anyhow::anyhow!(
            "Selected provisioning profile {} failed final CMS verification",
            best_path.display()
        )
    })
}

fn decode_cms_plist(path: &Path) -> Result<Value> {
    let out = Command::new("security")
        .args(["cms", "-D", "-i"])
        .arg(path)
        .output()?;
    if !out.status.success() {
        bail!("security cms -D failed for {}", path.display());
    }
    Ok(plist::from_bytes(&out.stdout)?)
}

fn decode_profile_meta_via_cms(
    path: &Path,
    mtime_secs: u64,
    size: u64,
) -> Option<CachedProfileMeta> {
    let val = decode_cms_plist(path).ok()?;
    let dict = val.as_dictionary()?;
    let ent = dict.get("Entitlements")?.as_dictionary()?;
    let teams: Vec<String> = dict
        .get("TeamIdentifier")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_string().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let provisioned_devices: Vec<String> = dict
        .get("ProvisionedDevices")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_string().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let cert_sha1s: Vec<String> = dict
        .get("DeveloperCertificates")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.as_data().map(sha1_compat::sha1_hex_upper))
                .collect()
        })
        .unwrap_or_default();

    Some(CachedProfileMeta {
        mtime_secs,
        size,
        name: dict
            .get("Name")
            .and_then(|v| v.as_string())
            .unwrap_or("Unknown")
            .to_string(),
        teams,
        app_id: ent
            .get("application-identifier")
            .and_then(|v| v.as_string())
            .unwrap_or("")
            .to_string(),
        get_task_allow: ent
            .get("get-task-allow")
            .and_then(|v| v.as_boolean())
            .unwrap_or(false),
        provisions_all_devices: dict
            .get("ProvisionsAllDevices")
            .and_then(|v| v.as_boolean())
            .unwrap_or(false),
        expiration_rfc3339: dict
            .get("ExpirationDate")
            .and_then(|v| v.as_date())
            .map(|d| d.to_xml_format())
            .unwrap_or_default(),
        provisioned_devices,
        cert_sha1s,
    })
}

fn cms_decode_and_match_profile(
    path: &Path,
    bundle_id: &str,
    team_id: &str,
    _target_udid: Option<&str>,
    installed_ids: &[(String, String)],
    explicit_identity: Option<&str>,
) -> Result<Option<MatchedProfile>> {
    let val = decode_cms_plist(path)?;
    let Some(dict) = val.as_dictionary() else {
        return Ok(None);
    };
    let Some(ent) = dict.get("Entitlements").and_then(|v| v.as_dictionary()) else {
        return Ok(None);
    };
    let app_id = ent
        .get("application-identifier")
        .and_then(|v| v.as_string())
        .unwrap_or("");
    let exact = format!("{team_id}.{bundle_id}");
    let wildcard = format!("{team_id}.*");
    if app_id != exact && app_id != wildcard {
        return Ok(None);
    }

    let profile_certs: Vec<String> = dict
        .get("DeveloperCertificates")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.as_data().map(sha1_compat::sha1_hex_upper))
                .collect()
        })
        .unwrap_or_default();

    let chosen_sha1 = if let Some(exp_id) = explicit_identity {
        exp_id.to_uppercase()
    } else if let Some((sha1, _)) = installed_ids
        .iter()
        .find(|(sha1, _)| profile_certs.iter().any(|pc| pc == sha1))
    {
        sha1.clone()
    } else {
        return Ok(None);
    };

    let name = dict
        .get("Name")
        .and_then(|v| v.as_string())
        .unwrap_or("Unknown")
        .to_string();
    let get_task_allow = ent
        .get("get-task-allow")
        .and_then(|v| v.as_boolean())
        .unwrap_or(false);
    let provisions_all_devices = dict
        .get("ProvisionsAllDevices")
        .and_then(|v| v.as_boolean())
        .unwrap_or(false);
    let device_count = dict
        .get("ProvisionedDevices")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let kind = classify_profile(get_task_allow, provisions_all_devices, device_count);

    Ok(Some(MatchedProfile {
        path: path.to_path_buf(),
        name,
        kind,
        cert_sha1: chosen_sha1,
        entitlements: ent.clone(),
    }))
}

mod sha1_compat {
    pub fn sha1_hex_upper(data: &[u8]) -> String {
        let mut h0: u32 = 0x67452301;
        let mut h1: u32 = 0xEFCDAB89;
        let mut h2: u32 = 0x98BADCFE;
        let mut h3: u32 = 0x10325476;
        let mut h4: u32 = 0xC3D2E1F0;

        let bit_len = (data.len() as u64) * 8;
        let mut msg = data.to_vec();
        msg.push(0x80);
        while (msg.len() % 64) != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bit_len.to_be_bytes());

        for chunk in msg.as_chunks::<64>().0 {
            let mut w = [0u32; 80];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([
                    chunk[4 * i],
                    chunk[4 * i + 1],
                    chunk[4 * i + 2],
                    chunk[4 * i + 3],
                ]);
            }
            for i in 16..80 {
                w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
            }
            let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
            for (i, &wi) in w.iter().enumerate() {
                let (f, k) = match i {
                    0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                    20..=39 => (b ^ c ^ d, 0x6ED9EBA1u32),
                    40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDCu32),
                    _ => (b ^ c ^ d, 0xCA62C1D6u32),
                };
                let temp = a
                    .rotate_left(5)
                    .wrapping_add(f)
                    .wrapping_add(e)
                    .wrapping_add(k)
                    .wrapping_add(wi);
                e = d;
                d = c;
                c = b.rotate_left(30);
                b = a;
                a = temp;
            }
            h0 = h0.wrapping_add(a);
            h1 = h1.wrapping_add(b);
            h2 = h2.wrapping_add(c);
            h3 = h3.wrapping_add(d);
            h4 = h4.wrapping_add(e);
        }

        format!("{h0:08X}{h1:08X}{h2:08X}{h3:08X}{h4:08X}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_rfc_vector() {
        assert_eq!(
            sha1_compat::sha1_hex_upper(b"abc"),
            "A9993E364706816ABA3E25717850C26C9CD0D89D"
        );
    }

    #[test]
    fn parses_security_find_identity_output() {
        let sample = r#"  1) AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA "Apple Development: Test User (ABCDE12345)"
  2) BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB "Developer ID Application: Test Corp (TEAM123456)"
     2 valid identities found"#;
        let ids = parse_codesign_identities(sample);
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0].0, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
        assert!(ids[0].1.contains("Apple Development"));
    }

    #[test]
    fn classifies_provisioning_profile_types() {
        assert_eq!(classify_profile(true, false, 5), ProfileType::Development);
        assert_eq!(classify_profile(false, false, 5), ProfileType::AdHoc);
        assert_eq!(classify_profile(false, true, 0), ProfileType::Enterprise);
        assert_eq!(classify_profile(false, false, 0), ProfileType::AppStore);
    }
}
