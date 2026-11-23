use serde_json::{Map, Value};
use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XcodeProjMetadata {
    pub target_name: Option<String>,
    pub scheme: Option<String>,
    pub app_name: Option<String>,
    pub bundle_id: Option<String>,
    pub team_id: Option<String>,
    pub app_icon: Option<String>,
    pub deployment_target: Option<String>,
}

/// Inspect a local `*.xcodeproj` bundle using direct JSON parsing (for JSON `project.pbxproj`
/// or Xcode 27.2 `project.xcproj`) or macOS `plutil -convert json -o -` (for OpenStep ASCII
/// `project.pbxproj`). Returns `None` gracefully if the project cannot be parsed or `plutil`
/// is unavailable on the host OS.
pub fn inspect_xcodeproj(xcodeproj_dir: &Path) -> Option<XcodeProjMetadata> {
    let project_stem = xcodeproj_dir.file_stem()?.to_str()?;

    let proj_file = [
        xcodeproj_dir.join("project.pbxproj"),
        xcodeproj_dir.join("project.xcproj"),
    ]
    .into_iter()
    .find(|p| p.is_file())?;

    let json = load_xcodeproj_json(&proj_file)?;
    let mut meta = parse_pbxproj_json(&json, project_stem)?;

    if let Some(shared_scheme) =
        detect_shared_scheme(xcodeproj_dir, meta.target_name.as_deref(), project_stem)
    {
        meta.scheme = Some(shared_scheme);
    }

    Some(meta)
}

fn load_xcodeproj_json(proj_file: &Path) -> Option<Value> {
    let bytes = fs::read(proj_file).ok()?;
    let trimmed = bytes.iter().copied().find(|b| !b.is_ascii_whitespace());

    if trimmed == Some(b'{') {
        return serde_json::from_slice(&bytes).ok();
    }

    let out = Command::new("plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(proj_file)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Parse the JSON object graph of a `project.pbxproj` (as emitted by `plutil -convert json` or
/// stored directly as JSON), merging project-level and main iOS application target build settings.
pub fn parse_pbxproj_json(root: &Value, project_stem: &str) -> Option<XcodeProjMetadata> {
    let objects = root.get("objects")?.as_object()?;
    let root_id = root.get("rootObject")?.as_str()?;
    let root_obj = objects.get(root_id)?.as_object()?;

    let project_bs = root_obj
        .get("buildConfigurationList")
        .and_then(Value::as_str)
        .and_then(|id| pick_build_settings(objects, id));

    let app_target = find_main_app_target(objects, root_obj, project_stem);

    let target_name = app_target
        .and_then(|t| t.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let target_bs = app_target
        .and_then(|t| t.get("buildConfigurationList"))
        .and_then(Value::as_str)
        .and_then(|id| pick_build_settings(objects, id));

    let raw_product_name = get_setting(target_bs, project_bs, "PRODUCT_NAME");
    let app_name = resolve_product_name(raw_product_name, target_name.as_deref(), project_stem)
        .or_else(|| target_name.clone());

    let bundle_id = clean_setting(get_setting(
        target_bs,
        project_bs,
        "PRODUCT_BUNDLE_IDENTIFIER",
    ));
    let team_id = clean_setting(get_setting(target_bs, project_bs, "DEVELOPMENT_TEAM"));
    let app_icon = clean_setting(get_setting(
        target_bs,
        project_bs,
        "ASSETCATALOG_COMPILER_APPICON_NAME",
    ));
    let deployment_target = clean_setting(get_setting(
        target_bs,
        project_bs,
        "IPHONEOS_DEPLOYMENT_TARGET",
    ));

    Some(XcodeProjMetadata {
        scheme: target_name.clone(),
        target_name,
        app_name,
        bundle_id,
        team_id,
        app_icon,
        deployment_target,
    })
}

fn find_main_app_target<'a>(
    objects: &'a Map<String, Value>,
    root_obj: &'a Map<String, Value>,
    project_stem: &str,
) -> Option<&'a Map<String, Value>> {
    let target_ids = root_obj.get("targets")?.as_array()?;
    let mut app_targets: Vec<&Map<String, Value>> = Vec::new();

    for tid in target_ids {
        let Some(id) = tid.as_str() else {
            continue;
        };
        let Some(target) = objects.get(id).and_then(Value::as_object) else {
            continue;
        };
        let isa = target.get("isa").and_then(Value::as_str);
        let product_type = target.get("productType").and_then(Value::as_str);
        if isa == Some("PBXNativeTarget")
            && product_type == Some("com.apple.product-type.application")
        {
            app_targets.push(target);
        }
    }

    app_targets
        .iter()
        .copied()
        .find(|t| t.get("name").and_then(Value::as_str) == Some(project_stem))
        .or_else(|| app_targets.into_iter().next())
}

fn pick_build_settings<'a>(
    objects: &'a Map<String, Value>,
    config_list_id: &str,
) -> Option<&'a Map<String, Value>> {
    let cfg_list = objects.get(config_list_id)?.as_object()?;
    let cfg_ids = cfg_list.get("buildConfigurations")?.as_array()?;

    let mut configs: Vec<(&str, &Map<String, Value>)> = Vec::new();
    for cid in cfg_ids {
        let Some(id) = cid.as_str() else {
            continue;
        };
        let Some(cfg) = objects.get(id).and_then(Value::as_object) else {
            continue;
        };
        let Some(bs) = cfg.get("buildSettings").and_then(Value::as_object) else {
            continue;
        };
        let name = cfg.get("name").and_then(Value::as_str).unwrap_or("");
        configs.push((name, bs));
    }

    configs
        .iter()
        .find(|(name, _)| *name == "Debug")
        .or_else(|| configs.iter().find(|(name, _)| *name == "Release"))
        .or_else(|| configs.first())
        .map(|(_, bs)| *bs)
}

fn get_setting<'a>(
    target_bs: Option<&'a Map<String, Value>>,
    project_bs: Option<&'a Map<String, Value>>,
    key: &str,
) -> Option<&'a str> {
    target_bs
        .and_then(|bs| bs.get(key))
        .and_then(Value::as_str)
        .or_else(|| {
            project_bs
                .and_then(|bs| bs.get(key))
                .and_then(Value::as_str)
        })
}

fn clean_setting(val: Option<&str>) -> Option<String> {
    let s = val?.trim();
    if s.is_empty() || s.contains("$(") || s.contains("${") {
        return None;
    }
    Some(s.to_string())
}

fn resolve_product_name(
    raw: Option<&str>,
    target_name: Option<&str>,
    project_stem: &str,
) -> Option<String> {
    let s = raw?.trim();
    if s.is_empty() {
        return None;
    }
    if s == "$(TARGET_NAME)" || s == "${TARGET_NAME}" {
        return target_name.map(str::to_string);
    }
    if s == "$(PROJECT_NAME)" || s == "${PROJECT_NAME}" {
        return Some(project_stem.to_string());
    }
    clean_setting(Some(s))
}

fn detect_shared_scheme(
    xcodeproj_dir: &Path,
    target_name: Option<&str>,
    project_stem: &str,
) -> Option<String> {
    let schemes_dir = xcodeproj_dir.join("xcshareddata/xcschemes");
    let entries = fs::read_dir(&schemes_dir).ok()?;
    let mut schemes: Vec<String> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("xcscheme"))
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .collect();
    schemes.sort();

    if let Some(t) = target_name
        && schemes.iter().any(|s| s == t)
    {
        return Some(t.to_string());
    }
    if schemes.iter().any(|s| s == project_stem) {
        return Some(project_stem.to_string());
    }
    if schemes.len() == 1 {
        return schemes.into_iter().next();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn parses_pbxproj_json_with_project_and_target_inheritance() {
        let doc = json!({
            "rootObject": "ROOT",
            "objects": {
                "ROOT": {
                    "isa": "PBXProject",
                    "buildConfigurationList": "PROJ_CFGS",
                    "targets": ["TGT_ALT", "TGT_MAIN", "TGT_TESTS"]
                },
                "PROJ_CFGS": {
                    "isa": "XCConfigurationList",
                    "buildConfigurations": ["PROJ_DEBUG", "PROJ_RELEASE"]
                },
                "PROJ_DEBUG": {
                    "isa": "XCBuildConfiguration",
                    "name": "Debug",
                    "buildSettings": {
                        "DEVELOPMENT_TEAM": "TEAM987654",
                        "IPHONEOS_DEPLOYMENT_TARGET": "18.0",
                        "CODE_SIGN_STYLE": "Automatic"
                    }
                },
                "PROJ_RELEASE": {
                    "isa": "XCBuildConfiguration",
                    "name": "Release",
                    "buildSettings": {
                        "DEVELOPMENT_TEAM": "TEAM987654"
                    }
                },
                "TGT_ALT": {
                    "isa": "PBXNativeTarget",
                    "name": "HelperApp",
                    "productType": "com.apple.product-type.application",
                    "buildConfigurationList": "ALT_CFGS"
                },
                "ALT_CFGS": {
                    "isa": "XCConfigurationList",
                    "buildConfigurations": ["ALT_DEBUG"]
                },
                "ALT_DEBUG": {
                    "isa": "XCBuildConfiguration",
                    "name": "Debug",
                    "buildSettings": {
                        "PRODUCT_BUNDLE_IDENTIFIER": "com.example.helper"
                    }
                },
                "TGT_MAIN": {
                    "isa": "PBXNativeTarget",
                    "name": "DemoApp",
                    "productType": "com.apple.product-type.application",
                    "buildConfigurationList": "MAIN_CFGS"
                },
                "MAIN_CFGS": {
                    "isa": "XCConfigurationList",
                    "buildConfigurations": ["MAIN_DEBUG"]
                },
                "MAIN_DEBUG": {
                    "isa": "XCBuildConfiguration",
                    "name": "Debug",
                    "buildSettings": {
                        "PRODUCT_NAME": "$(TARGET_NAME)",
                        "PRODUCT_BUNDLE_IDENTIFIER": "com.example.demoapp",
                        "ASSETCATALOG_COMPILER_APPICON_NAME": "AppIcon"
                    }
                },
                "TGT_TESTS": {
                    "isa": "PBXNativeTarget",
                    "name": "DemoAppTests",
                    "productType": "com.apple.product-type.bundle.unit-test",
                    "buildConfigurationList": "TEST_CFGS"
                },
                "TEST_CFGS": {
                    "isa": "XCConfigurationList",
                    "buildConfigurations": ["TEST_DEBUG"]
                },
                "TEST_DEBUG": {
                    "isa": "XCBuildConfiguration",
                    "name": "Debug",
                    "buildSettings": {
                        "PRODUCT_BUNDLE_IDENTIFIER": "com.example.demoapp.tests"
                    }
                }
            }
        });

        let meta = parse_pbxproj_json(&doc, "DemoApp").expect("should parse");
        assert_eq!(meta.target_name.as_deref(), Some("DemoApp"));
        assert_eq!(meta.scheme.as_deref(), Some("DemoApp"));
        assert_eq!(meta.app_name.as_deref(), Some("DemoApp"));
        assert_eq!(meta.bundle_id.as_deref(), Some("com.example.demoapp"));
        assert_eq!(meta.team_id.as_deref(), Some("TEAM987654"));
        assert_eq!(meta.app_icon.as_deref(), Some("AppIcon"));
        assert_eq!(meta.deployment_target.as_deref(), Some("18.0"));
    }

    #[test]
    fn inspects_json_xcproj_directly_without_plutil() {
        let dir = tempdir().unwrap();
        let proj = dir.path().join("JsonApp.xcodeproj");
        fs::create_dir_all(proj.join("xcshareddata/xcschemes")).unwrap();
        fs::write(
            proj.join("xcshareddata/xcschemes/JsonAppScheme.xcscheme"),
            "<Scheme/>",
        )
        .unwrap();
        let doc = json!({
            "rootObject": "R1",
            "objects": {
                "R1": {
                    "isa": "PBXProject",
                    "buildConfigurationList": "PC",
                    "targets": ["T1"]
                },
                "PC": {
                    "isa": "XCConfigurationList",
                    "buildConfigurations": ["PD"]
                },
                "PD": {
                    "isa": "XCBuildConfiguration",
                    "name": "Debug",
                    "buildSettings": {
                        "DEVELOPMENT_TEAM": "ABCD123456"
                    }
                },
                "T1": {
                    "isa": "PBXNativeTarget",
                    "name": "JsonApp",
                    "productType": "com.apple.product-type.application",
                    "buildConfigurationList": "TC"
                },
                "TC": {
                    "isa": "XCConfigurationList",
                    "buildConfigurations": ["TD"]
                },
                "TD": {
                    "isa": "XCBuildConfiguration",
                    "name": "Debug",
                    "buildSettings": {
                        "PRODUCT_BUNDLE_IDENTIFIER": "com.example.jsonapp",
                        "ASSETCATALOG_COMPILER_APPICON_NAME": "PrimaryIcon",
                        "IPHONEOS_DEPLOYMENT_TARGET": "17.4"
                    }
                }
            }
        });
        fs::write(proj.join("project.xcproj"), doc.to_string()).unwrap();

        let meta = inspect_xcodeproj(&proj).expect("should inspect json xcproj");
        assert_eq!(meta.scheme.as_deref(), Some("JsonAppScheme"));
        assert_eq!(meta.bundle_id.as_deref(), Some("com.example.jsonapp"));
        assert_eq!(meta.team_id.as_deref(), Some("ABCD123456"));
        assert_eq!(meta.app_icon.as_deref(), Some("PrimaryIcon"));
        assert_eq!(meta.deployment_target.as_deref(), Some("17.4"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn inspects_openstep_pbxproj_via_plutil() {
        let dir = tempdir().unwrap();
        let proj = dir.path().join("ClassicApp.xcodeproj");
        fs::create_dir_all(&proj).unwrap();
        let pbx = r#"// !$*UTF8*$!
{
	archiveVersion = 1;
	objectVersion = 56;
	objects = {
		ROOT01 = {
			isa = PBXProject;
			buildConfigurationList = PCONF01;
			targets = (
				TGT01,
			);
		};
		PCONF01 = {
			isa = XCConfigurationList;
			buildConfigurations = (
				PDBG01,
			);
		};
		PDBG01 = {
			isa = XCBuildConfiguration;
			buildSettings = {
				DEVELOPMENT_TEAM = TEAM555555;
				IPHONEOS_DEPLOYMENT_TARGET = 18.2;
			};
			name = Debug;
		};
		TGT01 = {
			isa = PBXNativeTarget;
			buildConfigurationList = TCONF01;
			name = ClassicApp;
			productType = "com.apple.product-type.application";
		};
		TCONF01 = {
			isa = XCConfigurationList;
			buildConfigurations = (
				TDBG01,
			);
		};
		TDBG01 = {
			isa = XCBuildConfiguration;
			buildSettings = {
				ASSETCATALOG_COMPILER_APPICON_NAME = AppIcon;
				PRODUCT_BUNDLE_IDENTIFIER = com.example.classic;
				PRODUCT_NAME = "$(TARGET_NAME)";
			};
			name = Debug;
		};
	};
	rootObject = ROOT01;
}
"#;
        fs::write(proj.join("project.pbxproj"), pbx).unwrap();

        let meta = inspect_xcodeproj(&proj).expect("should parse via plutil");
        assert_eq!(meta.target_name.as_deref(), Some("ClassicApp"));
        assert_eq!(meta.app_name.as_deref(), Some("ClassicApp"));
        assert_eq!(meta.bundle_id.as_deref(), Some("com.example.classic"));
        assert_eq!(meta.team_id.as_deref(), Some("TEAM555555"));
        assert_eq!(meta.app_icon.as_deref(), Some("AppIcon"));
        assert_eq!(meta.deployment_target.as_deref(), Some("18.2"));
    }
}
