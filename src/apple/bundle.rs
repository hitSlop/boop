use crate::cli::BuildTarget;
use anyhow::{Context, Result, bail, ensure};
use plist::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct Bundle {
    path: PathBuf,
    exe: String,
    framework: bool,
    identifier: String,
}

impl Bundle {
    fn read(path: &Path, framework: bool) -> Result<Self> {
        let plist = Value::from_file(path.join("Info.plist"))
            .with_context(|| format!("reading {}/Info.plist", path.display()))?;
        let dict = plist.as_dictionary().context("invalid bundle plist")?;
        let exe = dict
            .get("CFBundleExecutable")
            .and_then(Value::as_string)
            .context("missing CFBundleExecutable")?;
        ensure!(
            Path::new(exe).file_name().and_then(|n| n.to_str()) == Some(exe),
            "invalid CFBundleExecutable {exe}"
        );
        Ok(Self {
            path: path.into(),
            exe: exe.into(),
            framework,
            identifier: dict
                .get("CFBundleIdentifier")
                .and_then(Value::as_string)
                .unwrap_or("")
                .into(),
        })
    }
    fn name(&self) -> Result<&str> {
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .context("invalid bundle name")
    }
    fn verify(&self, target: BuildTarget) -> Result<()> {
        let exe = self.path.join(&self.exe);
        let expected = match target {
            BuildTarget::Device => "IOS",
            BuildTarget::Simulator => "IOSSIMULATOR",
        };
        let v = checked(
            Command::new("xcrun")
                .args(["vtool", "-arch", "arm64", "-show-build"])
                .arg(&exe),
        )?;
        ensure!(
            v.lines()
                .any(|l| l.split_whitespace().eq(["platform", expected])),
            "{} requires arm64 {expected}: {v}",
            exe.display()
        );
        let hdr = checked(
            Command::new("otool")
                .args(["-arch", "arm64", "-hv"])
                .arg(&exe),
        )?;
        let kind = if self.framework { "DYLIB" } else { "EXECUTE" };
        ensure!(
            hdr.lines().any(|l| l.split_whitespace().any(|t| t == kind)),
            "{} requires a Mach-O {kind}",
            exe.display()
        );
        Ok(())
    }
    fn stub(&self) -> Result<()> {
        // Match Xcode's codeless-framework stub target, including its sentinel deployment version.
        let out = self.path.join(&self.exe);
        let temp = tempfile::NamedTempFile::new_in(&self.path)?;
        checked(
            Command::new("xcrun")
                .args([
                    "--sdk",
                    "iphonesimulator",
                    "clang",
                    "-dynamiclib",
                    "-target",
                    "arm64-apple-ios100.0-simulator",
                    "-Xlinker",
                    "-adhoc_codesign",
                    "-install_name",
                    &format!("@rpath/{}/{}", self.name()?, self.exe),
                    "-x",
                    "c",
                    "/dev/null",
                    "-o",
                ])
                .arg(temp.path()),
        )?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
        temp.persist(out)?;
        Ok(())
    }
}

fn checked(cmd: &mut Command) -> Result<String> {
    let res = cmd.output().with_context(|| format!("running {cmd:?}"))?;
    ensure!(
        res.status.success(),
        "{cmd:?}: {}{}",
        String::from_utf8_lossy(&res.stdout),
        String::from_utf8_lossy(&res.stderr)
    );
    Ok(String::from_utf8_lossy(&res.stdout).into_owned())
}

pub fn validate(
    app: &Path,
    target: BuildTarget,
    static_names: impl FnOnce(&BTreeMap<String, String>) -> Result<BTreeSet<String>>,
) -> Result<()> {
    let mut bundles = vec![Bundle::read(app, false)?];
    for entry in ignore::WalkBuilder::new(app)
        .standard_filters(false)
        .build()
    {
        let e = entry?;
        if e.depth() == 0 || !e.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        match e.path().extension().and_then(|s| s.to_str()) {
            Some("framework") => bundles.push(Bundle::read(e.path(), true)?),
            Some("appex" | "app") => bundles.push(Bundle::read(e.path(), false)?),
            _ => {}
        }
    }
    let mut missing = BTreeMap::new();
    for b in &bundles {
        if b.path.join(&b.exe).try_exists()? {
            b.verify(target)?;
        } else if target == BuildTarget::Simulator && b.framework {
            missing.insert(b.name()?.to_owned(), b.identifier.clone());
        } else {
            bail!(
                "{} is missing its bundle executable {}",
                b.path.display(),
                b.exe
            );
        }
    }
    if !missing.is_empty() {
        let allowed = static_names(&missing)?;
        for b in bundles.iter().filter(|b| !b.path.join(&b.exe).exists()) {
            ensure!(
                allowed.contains(b.name()?),
                "{} is missing its bundle executable {}; no verified static arm64 Simulator slice",
                b.path.display(),
                b.exe
            );
            b.stub()?;
            b.verify(target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(target_os = "macos")]
    fn simulator_repairs_only_verified_static_wrappers() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("Test.app");
        let fw = app.join("Frameworks/Vendor.framework");
        fs::create_dir_all(&fw).unwrap();
        for (path, name) in [(&app, "Test"), (&fw, "Vendor")] {
            let mut dict = plist::Dictionary::new();
            dict.insert("CFBundleExecutable".into(), Value::String(name.into()));
            dict.insert(
                "CFBundleIdentifier".into(),
                Value::String(format!("example.{name}")),
            );
            Value::Dictionary(dict)
                .to_file_xml(path.join("Info.plist"))
                .unwrap();
        }
        let src = dir.path().join("main.c");
        fs::write(&src, "int main(void) { return 0; }").unwrap();
        checked(
            Command::new("xcrun")
                .args([
                    "--sdk",
                    "iphonesimulator",
                    "clang",
                    "-target",
                    "arm64-apple-ios17.0-simulator",
                ])
                .arg(&src)
                .arg("-o")
                .arg(app.join("Test")),
        )
        .unwrap();
        fs::write(fw.join("resource"), "resource").unwrap();
        assert!(validate(&app, BuildTarget::Simulator, |_| Ok(BTreeSet::new())).is_err());
        assert!(!fw.join("Vendor").exists());
        validate(&app, BuildTarget::Simulator, |missing| {
            Ok(missing.keys().cloned().collect())
        })
        .unwrap();
        assert_eq!(fs::read(fw.join("resource")).unwrap(), b"resource");
        assert!(validate(&app, BuildTarget::Device, |_| panic!("wrong platform")).is_err());
        let names = checked(Command::new("otool").arg("-D").arg(fw.join("Vendor"))).unwrap();
        assert!(names.contains("@rpath/Vendor.framework/Vendor"));
    }

    // Run against a disposable copy of a real pulled app and its dependency checkout.
    #[test]
    #[ignore = "requires BOOP_VERIFY_APP and BOOP_VERIFY_SOURCES"]
    fn validate_existing_simulator_artifact() {
        let app = PathBuf::from(std::env::var_os("BOOP_VERIFY_APP").unwrap());
        let sources = PathBuf::from(std::env::var_os("BOOP_VERIFY_SOURCES").unwrap());
        validate(&app, BuildTarget::Simulator, |missing| {
            let out = checked(
                Command::new("python3")
                    .arg(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/scripts/static-frameworks.py"
                    ))
                    .arg(serde_json::to_string(missing)?)
                    .arg("simulator")
                    .current_dir(&sources),
            )?;
            Ok(serde_json::from_str(&out)?)
        })
        .unwrap();
        crate::apple::sign::sign_for_simulator(&app).unwrap();
        checked(
            Command::new("codesign")
                .args(["--verify", "--deep", "--strict"])
                .arg(&app),
        )
        .unwrap();
    }
}
