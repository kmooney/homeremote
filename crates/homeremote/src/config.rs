//! Persisted configuration: device identities, pairing credentials, activities.
//!
//! Two rules shape this file. Devices are keyed by [`DeviceId`], never by IP —
//! a DHCP lease change must not break the remote. And credentials are written
//! once and reused: making the family re-approve the TV on every launch would
//! be worse than the Harmony we're replacing.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::device::{Command, DeviceId};
use crate::discovery::{Discovered, MacAddr, Vendor};
use crate::{Error, Result};

/// Where the daemon listens. Not 8080 or 3000, which collide with everything.
pub const DEFAULT_PORT: u16 = 8385;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(rename = "device")]
    pub devices: Vec<DeviceConfig>,
    #[serde(rename = "activity")]
    pub activities: Vec<Activity>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { port: DEFAULT_PORT }
    }
}

/// A device we've seen before, and whatever we need to talk to it again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub id: DeviceId,
    pub vendor: Vendor,
    pub name: String,
    /// Last known address. A hint for a fast start, re-checked by discovery —
    /// never the identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<IpAddr>,
    /// Control port for devices that advertise one; re-resolved by discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Needed to wake the TV, which stops answering IP in standby.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<MacAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<Credentials>,
}

impl DeviceConfig {
    pub fn from_discovered(found: &Discovered) -> Self {
        Self {
            id: found.id.clone(),
            vendor: found.vendor,
            name: found.name.clone(),
            addr: Some(found.addr),
            port: found.port,
            mac: found.mac,
            credentials: None,
        }
    }

    /// Rebuild the discovery record for a saved device, so it can be opened
    /// without re-scanning.
    ///
    /// Fails when we have no address for it — a device saved while it was in
    /// standby and never seen since.
    pub fn as_discovered(&self) -> Result<Discovered> {
        Ok(Discovered {
            id: self.id.clone(),
            vendor: self.vendor,
            name: self.name.clone(),
            model: None,
            addr: self
                .addr
                .ok_or_else(|| Error::Config(format!("no address known for {}", self.id)))?,
            port: self.port,
            mac: self.mac,
        })
    }

    /// Refresh the volatile fields from a fresh scan, keeping credentials and
    /// any MAC we already knew.
    pub fn refresh(&mut self, found: &Discovered) {
        self.addr = Some(found.addr);
        self.name = found.name.clone();
        if found.port.is_some() {
            self.port = found.port;
        }
        if found.mac.is_some() {
            self.mac = found.mac;
        }
    }
}

/// Secrets earned by pairing, which the user should not have to repeat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Credentials {
    /// Returned by the TV when someone taps Allow on screen.
    Samsung { token: String },
    /// Companion pairing credentials, hex-encoded.
    AppleTv {
        /// The Apple TV's long-term public key.
        device_public: String,
        /// Our long-term private key.
        client_private: String,
        /// The Apple TV's pairing identifier.
        device_id: String,
        /// The identifier we gave it.
        client_id: String,
    },
}

/// A Harmony-style one-button macro.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub steps: Vec<Step>,
    /// Where volume always goes while this activity is running — the receiver,
    /// which is the entire reason this project exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_device: Option<DeviceId>,
    /// Which device receives navigation and transport keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_device: Option<DeviceId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub device: DeviceId,
    pub command: Command,
    /// Wait this long *after* the step. The receiver ignores input commands for
    /// a couple of seconds after power-on, so this is load-bearing, not polish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
}

impl Config {
    pub fn device(&self, id: &DeviceId) -> Option<&DeviceConfig> {
        self.devices.iter().find(|d| &d.id == id)
    }

    pub fn device_mut(&mut self, id: &DeviceId) -> Option<&mut DeviceConfig> {
        self.devices.iter_mut().find(|d| &d.id == id)
    }

    pub fn activity(&self, id: &str) -> Option<&Activity> {
        self.activities.iter().find(|a| a.id == id)
    }

    /// Merge a scan into the config: refresh what we know, add what's new.
    ///
    /// Devices that didn't answer are kept, not pruned — a TV that was off
    /// during the scan is still a device we own.
    pub fn merge_discovered(&mut self, found: &[Discovered]) {
        for device in found {
            match self.device_mut(&device.id) {
                Some(existing) => existing.refresh(device),
                None => self.devices.push(DeviceConfig::from_discovered(device)),
            }
        }
        self.devices.sort_by(|a, b| a.id.cmp(&b.id));
    }

    /// The standard config location for this platform.
    ///
    /// `~/Library/Application Support/homeremote/config.toml` on macOS,
    /// `%APPDATA%\homeremote\config.toml` on Windows.
    pub fn default_path() -> Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("", "", "homeremote")
            .ok_or_else(|| Error::Config("could not determine a config directory".into()))?;
        Ok(dirs.config_dir().join("config.toml"))
    }

    /// Load from `path`, or return defaults if it doesn't exist yet.
    ///
    /// A missing file is a first run, not an error. A *malformed* file is an
    /// error: silently resetting would throw away pairing credentials.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(Error::Io(e)),
        };
        Self::from_toml(&text)
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| Error::Config(format!("invalid config: {e}")))
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|e| Error::Config(format!("cannot serialise: {e}")))
    }

    /// Write atomically, so an interrupted save can't leave a truncated file
    /// with the Samsung token half-written.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension("toml.tmp");
        std::fs::write(&temp, self.to_toml()?)?;
        std::fs::rename(&temp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{PowerState, VolumeCmd};
    use std::net::Ipv4Addr;

    fn found(id: &str, addr: [u8; 4], mac: Option<&str>) -> Discovered {
        Discovered {
            id: DeviceId::new(id),
            vendor: Vendor::Samsung,
            name: "Living Room".into(),
            model: None,
            addr: IpAddr::V4(Ipv4Addr::from(addr)),
            port: None,
            mac: mac.map(|m| m.parse().unwrap()),
        }
    }

    #[test]
    fn empty_config_round_trips() {
        let config = Config::default();
        let parsed = Config::from_toml(&config.to_toml().unwrap()).unwrap();
        assert_eq!(config, parsed);
        assert_eq!(parsed.server.port, DEFAULT_PORT);
    }

    #[test]
    fn full_config_round_trips() {
        let mut config = Config::default();
        config.devices.push(DeviceConfig {
            id: DeviceId::new("samsung:a0:b1:c2:d3:e4:f5"),
            vendor: Vendor::Samsung,
            name: "Living Room".into(),
            addr: Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20))),
            port: None,
            mac: Some("a0:b1:c2:d3:e4:f5".parse().unwrap()),
            credentials: Some(Credentials::Samsung {
                token: "12345678".into(),
            }),
        });
        config.activities.push(Activity {
            id: "apple-tv".into(),
            name: "Watch Apple TV".into(),
            steps: vec![
                Step {
                    device: DeviceId::new("denon:0005CD"),
                    command: Command::Power(PowerState::On),
                    delay_ms: Some(2500),
                },
                Step {
                    device: DeviceId::new("denon:0005CD"),
                    command: Command::Input("MPLAY".into()),
                    delay_ms: None,
                },
                Step {
                    device: DeviceId::new("denon:0005CD"),
                    command: Command::Volume(VolumeCmd::Set(0.35)),
                    delay_ms: None,
                },
            ],
            volume_device: Some(DeviceId::new("denon:0005CD")),
            remote_device: Some(DeviceId::new("appletv:AA:BB")),
        });

        let parsed = Config::from_toml(&config.to_toml().unwrap()).unwrap();
        assert_eq!(config, parsed, "config must survive a save/load cycle");
    }

    #[test]
    fn merge_adds_new_devices_and_refreshes_known_ones() {
        let mut config = Config::default();
        config.merge_discovered(&[found("samsung:aa", [192, 168, 1, 20], Some("aa:bb:cc:dd:ee:ff"))]);
        assert_eq!(config.devices.len(), 1);

        // Same device, new lease.
        config.merge_discovered(&[found("samsung:aa", [192, 168, 1, 77], None)]);
        assert_eq!(config.devices.len(), 1, "must not duplicate on re-scan");
        let device = &config.devices[0];
        assert_eq!(device.addr, Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 77))));
        assert!(device.mac.is_some(), "a scan without a MAC must not erase one");
    }

    #[test]
    fn merge_preserves_credentials() {
        let mut config = Config::default();
        config.merge_discovered(&[found("samsung:aa", [192, 168, 1, 20], None)]);
        config.devices[0].credentials = Some(Credentials::Samsung {
            token: "secret".into(),
        });

        config.merge_discovered(&[found("samsung:aa", [192, 168, 1, 21], None)]);
        assert!(
            config.devices[0].credentials.is_some(),
            "re-discovery must never cost the user their pairing"
        );
    }

    #[test]
    fn merge_keeps_devices_that_did_not_answer() {
        // The TV is off during this scan. It is still our TV.
        let mut config = Config::default();
        config.merge_discovered(&[found("samsung:aa", [192, 168, 1, 20], None)]);
        config.merge_discovered(&[found("denon:0005", [192, 168, 1, 50], None)]);
        assert_eq!(config.devices.len(), 2);
    }

    #[test]
    fn missing_file_is_a_first_run_not_an_error() {
        let path = Path::new("/nonexistent-dir-homeremote/config.toml");
        assert_eq!(Config::load(path).unwrap(), Config::default());
    }

    #[test]
    fn malformed_config_is_an_error() {
        // Resetting to defaults here would silently discard pairing credentials.
        assert!(Config::from_toml("this is not toml {{{").is_err());
    }
}
