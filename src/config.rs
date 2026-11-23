use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const PROJECT_CONFIG_FILE: &str = ".boop.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub project: ProjectSection,
    #[serde(default)]
    pub sync: SyncSection,
    #[serde(default)]
    pub signing: SigningSection,
    #[serde(default)]
    pub metal: MetalSection,
    #[serde(default)]
    pub assets: AssetsSection,
    #[serde(default)]
    pub distribution: DistributionSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSection {
    pub name: String,
    /// Build engine: "xtool" (default for Xcode/SwiftPM iOS apps) or "cargo"
    #[serde(default = "default_engine")]
    pub engine: String,
    /// Xcode scheme to build (e.g. "MyApp") for .xcodeproj projects
    pub scheme: Option<String>,
    /// App bundle name without .app (defaults to scheme or project name)
    pub app_name: Option<String>,
    /// iOS Bundle Identifier (e.g. "com.example.myapp")
    pub bundle_id: Option<String>,
    /// Apple Developer Team ID (e.g. "TEAM123456")
    pub team_id: Option<String>,
    /// Optional preferred worker name
    pub worker: Option<String>,
    /// Optional preferred physical device name or UDID
    pub default_device: Option<String>,
    /// Enable SwiftBuild content-addressable compilation cache (CAS) shared across worktrees
    #[serde(default)]
    pub compilation_cache: bool,
}

fn default_engine() -> String {
    "xtool".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncSection {
    /// Additional directory/file patterns to exclude from rsync
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SigningSection {
    /// "local-macos" (default, zero Apple creds on remote worker) or "none"
    #[serde(default = "default_signing_mode")]
    pub mode: String,
    /// Optional explicit codesign SHA1 identity override
    pub identity: Option<String>,
    /// Optional explicit .mobileprovision path override
    pub provisioning_profile: Option<String>,
    /// Disable code signing in remote project.pbxproj before running xtool2
    #[serde(default = "default_true")]
    pub disable_remote_codesign: bool,
}

fn default_signing_mode() -> String {
    "local-macos".to_string()
}

fn default_true() -> bool {
    true
}

impl Default for SigningSection {
    fn default() -> Self {
        Self {
            mode: default_signing_mode(),
            identity: None,
            provisioning_profile: None,
            disable_remote_codesign: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetalSection {
    /// Local shader directories to compile with `xcrun -sdk iphoneos metal` and inject into a bundle
    #[serde(default)]
    pub bundles: Vec<MetalBundleOverlay>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetalBundleOverlay {
    /// Relative path to folder containing .metal files (e.g. "Packages/AppUI/Sources/Design/Shaders")
    pub source_dir: String,
    /// Bundle path inside the .app (e.g. "AppUI_Design.bundle")
    pub target_bundle: String,
    /// Also copy raw .metal sources into the bundle alongside default.metallib
    #[serde(default = "default_true")]
    pub copy_sources: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AssetsSection {
    /// Asset catalogs (.xcassets) and/or Icon Composer bundles (.icon) compiled locally via hash-cached `xcrun actool`
    #[serde(default)]
    pub sources: Vec<String>,
    /// Primary app icon name passed to `--app-icon` (e.g. "AppIcon")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_icon: Option<String>,
    /// Pass `--include-all-app-icons` when multiple alternate icon bundles are listed
    #[serde(default)]
    pub include_all_app_icons: bool,
    /// Optional minimum deployment target override (defaults to Info.plist MinimumOSVersion or "17.0")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_deployment_target: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DistributionSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firebase: Option<FirebaseDistributionConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirebaseDistributionConfig {
    /// Firebase App ID (e.g. "1:000000000000:ios:..."). Auto-detected from GoogleService-Info.plist if omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Default Firebase tester group aliases (e.g. ["personal-testers"])
    #[serde(default)]
    pub groups: Vec<String>,
    /// Optional default individual tester emails
    #[serde(default)]
    pub testers: Vec<String>,
    /// Signing profile preference for publish: "auto" (prefer adhoc, fallback development), "adhoc", or "development"
    #[serde(default = "default_publish_signing_profile")]
    pub signing_profile: String,
    /// Optional explicit distribution codesign SHA1 identity override
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Optional explicit Ad Hoc .mobileprovision path override
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provisioning_profile: Option<String>,
}

fn default_publish_signing_profile() -> String {
    "auto".to_string()
}

impl Default for FirebaseDistributionConfig {
    fn default() -> Self {
        Self {
            app_id: None,
            groups: Vec::new(),
            testers: Vec::new(),
            signing_profile: default_publish_signing_profile(),
            identity: None,
            provisioning_profile: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub default_worker: Option<String>,
    #[serde(default)]
    pub workers: BTreeMap<String, WorkerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum WorkerConfig {
    Boat {
        sandbox_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pool: Option<PoolConfig>,
        #[serde(default = "default_machine_type")]
        machine_type: String,
        /// Estimated hourly cost in USD (default = $0.036, large = $0.072, xlarge = $0.20)
        #[serde(default = "default_boat_rate")]
        hourly_rate_usd: f64,
        /// Optional SSH private key override (defaults to ~/.ssh/ascii_box_ed25519)
        ssh_key: Option<String>,
    },
    Ssh {
        host: String,
        #[serde(default = "default_ssh_user")]
        user: String,
        #[serde(default = "default_ssh_port")]
        port: u16,
        ssh_key: Option<String>,
        #[serde(default)]
        hourly_rate_usd: f64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
    #[serde(default = "default_cap")]
    pub max_forks: std::num::NonZeroUsize,
}

fn default_cap() -> std::num::NonZeroUsize {
    std::num::NonZeroUsize::new(3).expect("three is nonzero")
}

fn default_machine_type() -> String {
    "default".to_string()
}

fn default_boat_rate() -> f64 {
    0.036
}

fn default_ssh_user() -> String {
    "user".to_string()
}

fn default_ssh_port() -> u16 {
    22
}

pub fn boat_hourly_rate(machine_type: &str) -> f64 {
    match machine_type {
        "small" => 0.018,
        "default" => 0.036,
        "large" => 0.072,
        "xlarge" => 0.200,
        _ => 0.036,
    }
}

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME environment variable is not set")
}

pub fn global_config_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config").join("boop").join("config.toml"))
}

pub fn state_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".local").join("state").join("boop"))
}

pub fn cache_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".cache").join("boop"))
}

pub fn load_global_config() -> Result<GlobalConfig> {
    let path = global_config_path()?;
    if !path.exists() {
        return Ok(GlobalConfig::default());
    }
    let raw =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("Failed to parse {}", path.display()))
}

pub fn save_global_config(cfg: &GlobalConfig) -> Result<PathBuf> {
    let path = global_config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let raw = toml::to_string_pretty(cfg)?;
    fs::write(&path, raw)?;
    Ok(path)
}

pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(PROJECT_CONFIG_FILE).exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

pub fn load_project_config(root: &Path) -> Result<ProjectConfig> {
    let path = root.join(PROJECT_CONFIG_FILE);
    let raw = fs::read_to_string(&path).with_context(|| {
        format!(
            "Could not read {}. Run `boop init` in your project root first.",
            path.display()
        )
    })?;
    toml::from_str(&raw).with_context(|| format!("Failed to parse {}", path.display()))
}

pub fn save_project_config(root: &Path, cfg: &ProjectConfig) -> Result<PathBuf> {
    let path = root.join(PROJECT_CONFIG_FILE);
    let raw = toml::to_string_pretty(cfg)?;
    fs::write(&path, raw)?;
    Ok(path)
}

pub fn resolve_worker<'a>(
    global: &'a GlobalConfig,
    cli_override: Option<&str>,
    project_override: Option<&str>,
) -> Result<(String, &'a WorkerConfig)> {
    let name = cli_override
        .or(project_override)
        .or(global.default_worker.as_deref())
        .or_else(|| global.workers.keys().next().map(|s| s.as_str()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No workers configured in ~/.config/boop/config.toml. Run `boop init` first."
            )
        })?;
    let worker = global.workers.get(name).ok_or_else(|| {
        anyhow::anyhow!(
            "Worker '{}' not found in ~/.config/boop/config.toml (available: {:?})",
            name,
            global.workers.keys().collect::<Vec<_>>()
        )
    })?;
    Ok((name.to_string(), worker))
}

/// Auto-detect project metadata from project.yml (XcodeGen), *.xcodeproj, xtool.yml, or Cargo.toml
pub fn detect_project(dir: &Path) -> Result<ProjectConfig> {
    let dir_name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("App")
        .to_string();

    // 1. XcodeGen project.yml
    let project_yml = dir.join("project.yml");
    if project_yml.exists() {
        let raw = fs::read_to_string(&project_yml).unwrap_or_default();
        let name = extract_yaml_scalar(&raw, "name:").unwrap_or_else(|| dir_name.clone());
        let bundle_id = extract_yaml_scalar(&raw, "PRODUCT_BUNDLE_IDENTIFIER:");
        let team_id = extract_yaml_scalar(&raw, "DEVELOPMENT_TEAM:");
        let metal_bundles = detect_metal_overlays(dir);
        let mut excludes = Vec::new();
        for noisy in [
            "_docs/",
            "_inspo/",
            "_issues/",
            "inspo/",
            "build/",
            "LocalModels/",
            "spikes/",
        ] {
            let trimmed = noisy.trim_end_matches('/');
            if dir.join(trimmed).exists() {
                excludes.push(noisy.to_string());
            }
        }
        let assets = detect_asset_catalogs(dir, &name);
        return Ok(ProjectConfig {
            project: ProjectSection {
                name: name.clone(),
                engine: "xtool".to_string(),
                scheme: Some(name.clone()),
                app_name: Some(name),
                bundle_id,
                team_id,
                worker: None,
                default_device: None,
                compilation_cache: false,
            },
            sync: SyncSection { exclude: excludes },
            signing: SigningSection::default(),
            metal: MetalSection {
                bundles: metal_bundles,
            },
            assets,
            distribution: detect_distribution(dir),
        });
    }

    // 2. Any *.xcodeproj
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("xcodeproj") {
                let stem = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&dir_name)
                    .to_string();
                let assets = detect_asset_catalogs(dir, &stem);
                return Ok(ProjectConfig {
                    project: ProjectSection {
                        name: stem.clone(),
                        engine: "xtool".to_string(),
                        scheme: Some(stem.clone()),
                        app_name: Some(stem),
                        bundle_id: None,
                        team_id: None,
                        worker: None,
                        default_device: None,
                        compilation_cache: false,
                    },
                    sync: SyncSection::default(),
                    signing: SigningSection::default(),
                    metal: MetalSection {
                        bundles: detect_metal_overlays(dir),
                    },
                    assets,
                    distribution: detect_distribution(dir),
                });
            }
        }
    }

    // 3. xtool SwiftPM project (Package.swift + optional xtool.yml)
    if dir.join("Package.swift").exists() {
        let mut bundle_id = None;
        let xtool_yml = dir.join("xtool.yml");
        if xtool_yml.exists() {
            let raw = fs::read_to_string(&xtool_yml).unwrap_or_default();
            bundle_id = extract_yaml_scalar(&raw, "bundleID:");
        }
        let assets = detect_asset_catalogs(dir, &dir_name);
        return Ok(ProjectConfig {
            project: ProjectSection {
                name: dir_name.clone(),
                engine: "xtool".to_string(),
                scheme: None,
                app_name: Some(dir_name),
                bundle_id,
                team_id: None,
                worker: None,
                default_device: None,
                compilation_cache: false,
            },
            sync: SyncSection::default(),
            signing: SigningSection::default(),
            metal: MetalSection {
                bundles: detect_metal_overlays(dir),
            },
            assets,
            distribution: detect_distribution(dir),
        });
    }

    // 4. Rust Cargo project
    if dir.join("Cargo.toml").exists() {
        return Ok(ProjectConfig {
            project: ProjectSection {
                name: dir_name,
                engine: "cargo".to_string(),
                scheme: None,
                app_name: None,
                bundle_id: None,
                team_id: None,
                worker: None,
                default_device: None,
                compilation_cache: false,
            },
            sync: SyncSection {
                exclude: vec!["target/".to_string()],
            },
            signing: SigningSection {
                mode: "none".to_string(),
                ..Default::default()
            },
            metal: MetalSection::default(),
            assets: AssetsSection::default(),
            distribution: DistributionSection::default(),
        });
    }

    bail!(
        "Could not detect an Xcode (.xcodeproj / project.yml), SwiftPM (Package.swift), or Cargo (Cargo.toml) project in {}",
        dir.display()
    )
}

fn extract_yaml_scalar(raw: &str, key: &str) -> Option<String> {
    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(key) {
            let val = rest.trim().trim_matches('\'').trim_matches('"');
            if !val.is_empty() && !val.starts_with('#') {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// Heuristic detector for SwiftPM targets containing .metal files (e.g. Packages/<Pkg>/Sources/<Target>/Shaders)
fn detect_metal_overlays(root: &Path) -> Vec<MetalBundleOverlay> {
    let mut overlays = Vec::new();
    let packages_dir = root.join("Packages");
    let Ok(pkgs) = fs::read_dir(&packages_dir) else {
        return overlays;
    };
    for pkg in pkgs.flatten() {
        let pkg_path = pkg.path();
        let pkg_name = match pkg_path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let sources = pkg_path.join("Sources");
        let Ok(targets) = fs::read_dir(&sources) else {
            continue;
        };
        for target in targets.flatten() {
            let target_path = target.path();
            let target_name = match target_path.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let shaders_dir = target_path.join("Shaders");
            if shaders_dir.is_dir()
                && dir_has_ext(&shaders_dir, "metal")
                && let Ok(rel) = shaders_dir.strip_prefix(root)
            {
                overlays.push(MetalBundleOverlay {
                    source_dir: rel.to_string_lossy().to_string(),
                    target_bundle: format!("{pkg_name}_{target_name}.bundle"),
                    copy_sources: true,
                });
            }
        }
    }
    overlays
}

fn dir_has_ext(dir: &Path, ext: &str) -> bool {
    fs::read_dir(dir)
        .ok()
        .map(|it| {
            it.flatten()
                .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some(ext))
        })
        .unwrap_or(false)
}

/// Auto-detect public `GOOGLE_APP_ID` from `GoogleService-Info.plist` if present in the project.
pub fn detect_firebase_app_id(root: &Path) -> Option<String> {
    let mut candidates = vec![
        root.join("GoogleService-Info.plist"),
        root.join("Resources/GoogleService-Info.plist"),
    ];
    if let Ok(apps) = fs::read_dir(root.join("Apps")) {
        for entry in apps.flatten() {
            let p = entry.path();
            if p.is_dir() {
                candidates.push(p.join("GoogleService-Info.plist"));
                candidates.push(p.join("Resources/GoogleService-Info.plist"));
            }
        }
    }
    for path in candidates {
        if path.is_file()
            && let Ok(val) = plist::Value::from_file(&path)
            && let Some(app_id) = val
                .as_dictionary()
                .and_then(|d| d.get("GOOGLE_APP_ID"))
                .and_then(plist::Value::as_string)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            return Some(app_id.to_string());
        }
    }
    None
}

fn detect_asset_catalogs(root: &Path, project_name: &str) -> AssetsSection {
    let mut sources = Vec::new();
    let mut app_icon = None;

    let icon_candidates = [
        format!("Apps/{project_name}/Resources/AppIcon.icon"),
        format!("Apps/{project_name}/AppIcon.icon"),
        "Resources/AppIcon.icon".to_string(),
        "AppIcon.icon".to_string(),
    ];
    for rel in icon_candidates {
        if root.join(&rel).exists() {
            sources.push(rel);
            app_icon = Some("AppIcon".to_string());
            break;
        }
    }

    let xcassets_candidates = [
        format!("Apps/{project_name}/Resources/Assets.xcassets"),
        format!("Apps/{project_name}/Assets.xcassets"),
        "Resources/Assets.xcassets".to_string(),
        "Assets.xcassets".to_string(),
    ];
    for rel in xcassets_candidates {
        let p = root.join(&rel);
        if p.is_dir() {
            if app_icon.is_none() && p.join("AppIcon.appiconset").is_dir() {
                app_icon = Some("AppIcon".to_string());
            }
            sources.push(rel);
            break;
        }
    }

    AssetsSection {
        sources,
        app_icon,
        include_all_app_icons: false,
        min_deployment_target: None,
    }
}

fn detect_distribution(root: &Path) -> DistributionSection {
    if let Some(app_id) = detect_firebase_app_id(root) {
        DistributionSection {
            firebase: Some(FirebaseDistributionConfig {
                app_id: Some(app_id),
                ..Default::default()
            }),
        }
    } else {
        DistributionSection::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn detects_xcodegen_project_and_metal_overlays() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("project.yml"),
            "name: MyApp\nsettings:\n  base:\n    DEVELOPMENT_TEAM: TEAM123456\n    PRODUCT_BUNDLE_IDENTIFIER: com.example.myapp\n",
        )
        .unwrap();
        let shaders = dir.path().join("Packages/AppUI/Sources/Design/Shaders");
        fs::create_dir_all(&shaders).unwrap();
        fs::write(shaders.join("Glow.metal"), "kernel void k() {}").unwrap();

        let cfg = detect_project(dir.path()).unwrap();
        assert_eq!(cfg.project.name, "MyApp");
        assert_eq!(cfg.project.bundle_id.as_deref(), Some("com.example.myapp"));
        assert_eq!(cfg.project.team_id.as_deref(), Some("TEAM123456"));
        assert_eq!(cfg.metal.bundles.len(), 1);
        assert_eq!(cfg.metal.bundles[0].target_bundle, "AppUI_Design.bundle");
    }

    #[test]
    fn detects_firebase_app_id_from_google_service_plist() {
        let dir = tempdir().unwrap();
        let res = dir.path().join("Apps/MyApp/Resources");
        fs::create_dir_all(&res).unwrap();
        let mut dict = plist::Dictionary::new();
        dict.insert(
            "GOOGLE_APP_ID".into(),
            plist::Value::String("1:123456:ios:abcdef".into()),
        );
        plist::Value::Dictionary(dict)
            .to_file_xml(res.join("GoogleService-Info.plist"))
            .unwrap();

        assert_eq!(
            detect_firebase_app_id(dir.path()).as_deref(),
            Some("1:123456:ios:abcdef")
        );
    }

    #[test]
    fn roundtrips_project_config_toml() {
        let dir = tempdir().unwrap();
        let cfg = ProjectConfig {
            project: ProjectSection {
                name: "Demo".into(),
                engine: "xtool".into(),
                scheme: Some("Demo".into()),
                app_name: Some("Demo".into()),
                bundle_id: Some("com.example.demo".into()),
                team_id: Some("TEAM123".into()),
                worker: Some("default".into()),
                default_device: Some("TestPhone".into()),
                compilation_cache: false,
            },
            sync: SyncSection {
                exclude: vec!["build/".into()],
            },
            signing: SigningSection::default(),
            metal: MetalSection::default(),
            assets: AssetsSection::default(),
            distribution: DistributionSection {
                firebase: Some(FirebaseDistributionConfig {
                    app_id: Some("1:123:ios:abc".into()),
                    groups: vec!["personal-testers".into()],
                    testers: vec![],
                    signing_profile: "auto".into(),
                    identity: None,
                    provisioning_profile: None,
                }),
            },
        };
        save_project_config(dir.path(), &cfg).unwrap();
        let loaded = load_project_config(dir.path()).unwrap();
        assert_eq!(loaded.project.name, "Demo");
        assert_eq!(
            loaded.project.bundle_id.as_deref(),
            Some("com.example.demo")
        );
        assert!(loaded.signing.disable_remote_codesign);
        let fb = loaded.distribution.firebase.unwrap();
        assert_eq!(fb.app_id.as_deref(), Some("1:123:ios:abc"));
        assert_eq!(fb.groups, vec!["personal-testers"]);
        assert_eq!(fb.signing_profile, "auto");
    }
}
