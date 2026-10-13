//! `config.json` handling, compatible with the Python version's file format.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

pub const DEFAULT_SPONSORBLOCK_API: &str = "https://sponsor.ajay.app/api/";
const WIKI: &str = "https://github.com/dmunozv04/iSponsorBlockTV/wiki";

/// Treat `null` like a missing key (the Python version may write nulls).
fn nullable<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

fn yes() -> bool {
    true
}
fn one() -> f64 {
    1.0
}
fn join_name() -> String {
    "iSponsorBlockTV".into()
}
fn sb_api() -> String {
    DEFAULT_SPONSORBLOCK_API.into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    #[serde(default, deserialize_with = "nullable")]
    pub screen_id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    /// Milliseconds to skip early, compensating for device latency.
    #[serde(default, deserialize_with = "nullable")]
    pub offset: f64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, deserialize_with = "nullable")]
    pub devices: Vec<Device>,
    #[serde(default, deserialize_with = "nullable")]
    pub apikey: String,
    /// `None` = absent (defaults to `["sponsor"]`), `Some([])` = explicitly skip nothing.
    #[serde(default)]
    pub skip_categories: Option<Vec<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub channel_whitelist: Vec<Channel>,
    #[serde(default = "yes")]
    pub skip_count_tracking: bool,
    #[serde(default)]
    pub mute_ads: bool,
    #[serde(default)]
    pub skip_ads: bool,
    #[serde(default = "one")]
    pub minimum_skip_length: f64,
    #[serde(default = "yes")]
    pub auto_play: bool,
    #[serde(default = "join_name")]
    pub join_name: String,
    #[serde(default)]
    pub use_proxy: bool,
    #[serde(default = "sb_api")]
    pub sponsorblock_api_url: String,
    /// Seconds to keep playing into a segment before skipping (0 = skip right away).
    #[serde(default, deserialize_with = "nullable")]
    pub skip_delay: f64,
    /// Seek to this many seconds before the segment ends (0 = skip to the very end).
    #[serde(default, deserialize_with = "nullable")]
    pub skip_end_margin: f64,
    /// Optional MQTT / Home Assistant notifications (off by default).
    #[serde(default, deserialize_with = "nullable")]
    pub mqtt: MqttConfig,
    /// Unknown keys are kept so saving doesn't drop settings from other versions.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn port_1883() -> u16 {
    1883
}
fn base_topic() -> String {
    "isponsorblocktv".into()
}
fn discovery_prefix() -> String {
    "homeassistant".into()
}

/// MQTT port as a number or numeric string (the Python version accepts both).
fn port<'de, D: Deserializer<'de>>(d: D) -> Result<u16, D::Error> {
    use serde::de::Error;
    match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => Ok(1883),
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .ok_or_else(|| D::Error::custom("mqtt.port must be between 1 and 65535")),
        Some(Value::String(s)) => s
            .trim()
            .parse()
            .map_err(|_| D::Error::custom("mqtt.port must be an integer")),
        Some(_) => Err(D::Error::custom("mqtt.port must be an integer")),
    }
}

/// `mqtt` block, same shape as the Python MQTT branch (see README).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MqttConfig {
    #[serde(default, deserialize_with = "nullable")]
    pub enabled: bool,
    #[serde(default, deserialize_with = "nullable")]
    pub broker: String,
    #[serde(default = "port_1883", deserialize_with = "port")]
    pub port: u16,
    #[serde(default, deserialize_with = "nullable")]
    pub username: String,
    #[serde(default, deserialize_with = "nullable")]
    pub password: String,
    #[serde(default, deserialize_with = "nullable")]
    pub tls: bool,
    #[serde(default = "base_topic")]
    pub base_topic: String,
    #[serde(default, deserialize_with = "nullable")]
    pub home_assistant: HomeAssistantConfig,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeAssistantConfig {
    /// Publish MQTT discovery so Home Assistant creates entities (only if MQTT is enabled).
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "discovery_prefix")]
    pub discovery_prefix: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for MqttConfig {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

impl Default for HomeAssistantConfig {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

impl Default for Config {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

/// Same location the Python version uses (`appdirs.user_data_dir("iSponsorBlockTV", "dmunozv04")`).
pub fn default_data_dir() -> PathBuf {
    let base = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
    if cfg!(windows) {
        base.join("dmunozv04").join("iSponsorBlockTV")
    } else {
        base.join("iSponsorBlockTV")
    }
}

impl Config {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("config.json")
    }

    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = Self::path(data_dir);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let mut cfg: Config = serde_json::from_str(&text)
                    .with_context(|| format!("parsing {}", path.display()))?;
                cfg.extra.remove("config_file");
                cfg.extra.remove("data_dir");
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!(
                    "No config file found, using defaults. Config will be saved to {}",
                    path.display()
                );
                Ok(Config::default())
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write the config, readable only by the current user (it holds the API key and screen ids).
    pub fn save(&self, data_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let json = serde_json::to_string_pretty(self)?;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            opts.mode(0o600);
            if path.exists() {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
        use std::io::Write;
        opts.open(&path)?.write_all(json.as_bytes())?;
        Ok(())
    }

    /// Checks done before `start` (port of `Config.validate`).
    pub fn validate(&mut self) -> Result<()> {
        if self.extra.contains_key("atvs") {
            bail!(
                "The atvs config option is deprecated and has stopped working. \
                 Please read this for more information on how to upgrade to V2:\n{WIKI}/Migrate-from-V1-to-V2"
            );
        }
        if self.devices.is_empty() {
            bail!("No devices found, please add at least one device (run `isponsorblocktv setup`)");
        }
        if let Some(d) = self.devices.iter().find(|d| d.screen_id.is_empty()) {
            bail!("Device {:?} has no screen id", d.name);
        }
        for d in self.devices.iter().filter(|d| d.screen_id.len() == 26) {
            tracing::warn!(
                "The screen id {} is 26 characters long, which is the old format. YouTube is revoking \
                 the old format screen ids; pair the device again to get the new id. See {WIKI}/new-screen-ID-format",
                d.screen_id
            );
        }
        if self.apikey.is_empty() && !self.channel_whitelist.is_empty() {
            bail!("No youtube API key found and channel whitelist is not empty");
        }
        if self.mqtt.enabled {
            if self.mqtt.broker.trim().is_empty() {
                bail!("mqtt.enabled is true but mqtt.broker is empty");
            }
            if self.mqtt.port == 0 {
                bail!("mqtt.port must be between 1 and 65535");
            }
        }
        if self.skip_categories.is_none() {
            println!("No categories found, using default: sponsor");
            self.skip_categories = Some(vec!["sponsor".into()]);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_python_config_and_keeps_unknown_keys() {
        let text = r#"{
            "devices": [{"screen_id": "abc", "name": null, "offset": 250, "custom": 1}],
            "apikey": null,
            "skip_categories": ["sponsor", "intro"],
            "channel_whitelist": [],
            "minimum_skip_length": 2,
            "future_option": "keep me"
        }"#;
        let cfg: Config = serde_json::from_str(text).unwrap();
        assert_eq!(cfg.devices[0].offset, 250.0);
        assert_eq!(cfg.devices[0].name, "");
        assert_eq!(cfg.apikey, "");
        assert!(cfg.skip_count_tracking && cfg.auto_play);
        assert_eq!(cfg.minimum_skip_length, 2.0);
        let out = serde_json::to_value(&cfg).unwrap();
        assert_eq!(out["future_option"], "keep me");
        assert_eq!(out["devices"][0]["custom"], 1);
    }

    #[test]
    fn mqtt_block_defaults_and_partial_config() {
        let cfg: Config = serde_json::from_str(r#"{"devices":[{"screen_id":"x"}]}"#).unwrap();
        assert!(!cfg.mqtt.enabled);
        assert_eq!(cfg.mqtt.port, 1883);
        assert_eq!(cfg.mqtt.base_topic, "isponsorblocktv");
        assert!(cfg.mqtt.home_assistant.enabled);
        assert_eq!(cfg.mqtt.home_assistant.discovery_prefix, "homeassistant");

        let mut cfg: Config = serde_json::from_str(
            r#"{"devices":[{"screen_id":"x"}],
                "mqtt":{"enabled":true,"broker":"mqtt.lan","port":"8883","home_assistant":{"enabled":false}}}"#,
        )
        .unwrap();
        assert_eq!(cfg.mqtt.port, 8883);
        assert!(!cfg.mqtt.home_assistant.enabled);
        assert_eq!(cfg.mqtt.home_assistant.discovery_prefix, "homeassistant");
        cfg.validate().unwrap();

        let mut cfg: Config =
            serde_json::from_str(r#"{"devices":[{"screen_id":"x"}],"mqtt":{"enabled":true}}"#)
                .unwrap();
        assert!(cfg.validate().is_err());
        assert!(serde_json::from_str::<Config>(r#"{"mqtt":{"port":70000}}"#).is_err());
    }

    #[test]
    fn missing_categories_default_to_sponsor() {
        let mut cfg: Config = serde_json::from_str(r#"{"devices":[{"screen_id":"x"}]}"#).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.skip_categories.unwrap(), vec!["sponsor"]);
    }
}
