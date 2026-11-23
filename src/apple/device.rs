use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Serialize)]
pub struct PhysicalDevice {
    pub name: String,
    pub udid: String,
    pub identifier: String,
    pub model: String,
    pub state: String,
    pub platform: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SimDevice {
    pub name: String,
    pub udid: String,
    pub state: String,
}

pub fn list_physical_devices() -> Result<Vec<PhysicalDevice>> {
    let tmp = std::env::temp_dir().join(format!("boop-devicectl-{}.json", uuid::Uuid::new_v4()));
    let out = Command::new("xcrun")
        .args(["devicectl", "list", "devices", "--json-output"])
        .arg(&tmp)
        .output()
        .context("Failed to execute `xcrun devicectl list devices`")?;
    if !out.status.success() {
        let _ = fs::remove_file(&tmp);
        bail!(
            "`xcrun devicectl list devices` failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let raw = fs::read_to_string(&tmp)?;
    let _ = fs::remove_file(&tmp);
    parse_devicectl_json(&raw)
}

pub fn parse_devicectl_json(raw: &str) -> Result<Vec<PhysicalDevice>> {
    let val: Value = serde_json::from_str(raw)?;
    let mut devices = Vec::new();
    if let Some(arr) = val["result"]["devices"].as_array() {
        for d in arr {
            let reality = d["hardwareProperties"]["reality"].as_str().unwrap_or("");
            if reality != "physical" {
                continue;
            }
            let platform = d["hardwareProperties"]["platform"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let name = d["deviceProperties"]["name"]
                .as_str()
                .unwrap_or("Unknown")
                .to_string();
            let udid = d["hardwareProperties"]["udid"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let identifier = d["identifier"].as_str().unwrap_or(&udid).to_string();
            let model = d["hardwareProperties"]["marketingName"]
                .as_str()
                .or_else(|| d["hardwareProperties"]["productType"].as_str())
                .unwrap_or("")
                .to_string();
            let state = d["connectionProperties"]["tunnelState"]
                .as_str()
                .or_else(|| d["connectionProperties"]["pairingState"].as_str())
                .unwrap_or("unknown")
                .to_string();
            devices.push(PhysicalDevice {
                name,
                udid,
                identifier,
                model,
                state,
                platform,
            });
        }
    }
    Ok(devices)
}

pub fn resolve_physical_device(selector: Option<&str>) -> Result<PhysicalDevice> {
    let devices = list_physical_devices()?;
    let iphones: Vec<PhysicalDevice> = devices
        .into_iter()
        .filter(|d| {
            d.platform.eq_ignore_ascii_case("iOS")
                || d.model.contains("iPhone")
                || d.model.contains("iPad")
        })
        .collect();

    if let Some(sel) = selector
        && sel != "auto"
    {
        if let Some(found) = iphones.iter().find(|d| {
            d.name.eq_ignore_ascii_case(sel)
                || d.udid.eq_ignore_ascii_case(sel)
                || d.identifier.eq_ignore_ascii_case(sel)
        }) {
            return Ok(found.clone());
        }
        bail!(
            "Physical iOS device '{}' not found. Available physical iOS devices: {:?}",
            sel,
            iphones
                .iter()
                .map(|d| format!("{} ({}, {})", d.name, d.udid, d.state))
                .collect::<Vec<_>>()
        );
    }

    // Prefer connected/available iPhone
    if let Some(avail) = iphones
        .iter()
        .find(|d| matches!(d.state.as_str(), "connected" | "available" | "paired"))
    {
        return Ok(avail.clone());
    }

    iphones.into_iter().next().ok_or_else(|| {
        anyhow::anyhow!("No paired physical iPhone found via `xcrun devicectl list devices`")
    })
}

pub fn install_and_launch_device(
    device: &PhysicalDevice,
    app_bundle: &Path,
    bundle_id: &str,
    launch: bool,
) -> Result<()> {
    let dev_arg = if !device.udid.is_empty() {
        &device.udid
    } else {
        &device.identifier
    };

    let install = Command::new("xcrun")
        .args(["devicectl", "device", "install", "app", "--device", dev_arg])
        .arg(app_bundle)
        .output()
        .context("Failed to run `xcrun devicectl device install app`")?;
    if !install.status.success() {
        bail!(
            "Device install failed: {}{}",
            String::from_utf8_lossy(&install.stdout),
            String::from_utf8_lossy(&install.stderr)
        );
    }

    if launch {
        let res = Command::new("xcrun")
            .args([
                "devicectl",
                "device",
                "process",
                "launch",
                "--terminate-existing",
                "--device",
                dev_arg,
                bundle_id,
            ])
            .output()
            .context("Failed to run `xcrun devicectl device process launch`")?;
        if !res.status.success() {
            bail!(
                "Device launch failed: {}{}",
                String::from_utf8_lossy(&res.stdout),
                String::from_utf8_lossy(&res.stderr)
            );
        }
    }
    Ok(())
}

pub fn resolve_simulator(selector: &str) -> Result<SimDevice> {
    let out = Command::new("xcrun")
        .args(["simctl", "list", "devices", "-j"])
        .output()
        .context("Failed to run `xcrun simctl list devices -j`")?;
    if !out.status.success() {
        bail!("`xcrun simctl list devices -j` failed");
    }
    let val: Value = serde_json::from_slice(&out.stdout)?;
    let mut all = Vec::new();
    if let Some(map) = val["devices"].as_object() {
        for (_runtime, arr) in map {
            if let Some(list) = arr.as_array() {
                for d in list {
                    if d["isAvailable"].as_bool() == Some(false) {
                        continue;
                    }
                    all.push(SimDevice {
                        name: d["name"].as_str().unwrap_or("").to_string(),
                        udid: d["udid"].as_str().unwrap_or("").to_string(),
                        state: d["state"].as_str().unwrap_or("").to_string(),
                    });
                }
            }
        }
    }

    if selector == "booted" {
        if let Some(b) = all.iter().find(|s| s.state == "Booted") {
            return Ok(b.clone());
        }
        bail!("No booted iOS Simulator found. Boot one or pass `--simulator <name-or-udid>`.");
    }

    all.into_iter()
        .find(|s| s.name.eq_ignore_ascii_case(selector) || s.udid.eq_ignore_ascii_case(selector))
        .ok_or_else(|| anyhow::anyhow!("Simulator '{selector}' not found"))
}

pub fn install_and_launch_simulator(
    sim: &SimDevice,
    app_bundle: &Path,
    bundle_id: &str,
    launch: bool,
) -> Result<()> {
    if sim.state != "Booted" {
        let boot = Command::new("xcrun")
            .args(["simctl", "boot", &sim.udid])
            .output()?;
        if !boot.status.success() {
            bail!(
                "Simulator boot failed: {}",
                String::from_utf8_lossy(&boot.stderr)
            );
        }
    }
    let install = Command::new("xcrun")
        .args(["simctl", "install", &sim.udid])
        .arg(app_bundle)
        .output()
        .context("Failed to run `xcrun simctl install`")?;
    if !install.status.success() {
        bail!(
            "Simulator install failed: {}",
            String::from_utf8_lossy(&install.stderr)
        );
    }
    if launch {
        let res = Command::new("xcrun")
            .args(["simctl", "launch", &sim.udid, bundle_id])
            .output()
            .context("Failed to run `xcrun simctl launch`")?;
        if !res.status.success() {
            bail!(
                "Simulator launch failed: {}",
                String::from_utf8_lossy(&res.stderr)
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_devicectl_physical_iphone() {
        let sample = r#"{
          "result": {
            "devices": [
              {
                "identifier": "UUID-1",
                "deviceProperties": { "name": "TestPhone" },
                "hardwareProperties": {
                  "reality": "physical",
                  "platform": "iOS",
                  "udid": "00000000-0000000000000001",
                  "marketingName": "iPhone 14 Pro"
                },
                "connectionProperties": { "tunnelState": "connected" }
              },
              {
                "identifier": "UUID-2",
                "deviceProperties": { "name": "Sim" },
                "hardwareProperties": { "reality": "simulated", "platform": "iOS" },
                "connectionProperties": { "tunnelState": "connected" }
              }
            ]
          }
        }"#;
        let devs = parse_devicectl_json(sample).unwrap();
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].name, "TestPhone");
        assert_eq!(devs[0].udid, "00000000-0000000000000001");
    }
}
