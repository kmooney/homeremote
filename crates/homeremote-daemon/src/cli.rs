//! Turning command-line words into device commands, and names into devices.
//!
//! This exists so every backend can be exercised against real hardware long
//! before there is a UI to blame.

use anyhow::{Context, Result, anyhow, bail};
use homeremote::config::{Config, DeviceConfig};
use homeremote::device::{Command, NavKey, PowerState, TransportKey, VolumeCmd};

/// Find the one device matching `needle`, by id or name, case-insensitively.
///
/// An ambiguous match is an error rather than a guess: picking the wrong device
/// here means turning off the wrong thing.
pub fn resolve<'a>(config: &'a Config, needle: &str) -> Result<&'a DeviceConfig> {
    let needle = needle.to_ascii_lowercase();
    let matches: Vec<&DeviceConfig> = config
        .devices
        .iter()
        .filter(|d| {
            d.id.as_str().to_ascii_lowercase().contains(&needle)
                || d.name.to_ascii_lowercase().contains(&needle)
                || format!("{:?}", d.vendor).to_ascii_lowercase() == needle
        })
        .collect();

    match matches.as_slice() {
        [one] => Ok(one),
        [] => {
            let known: Vec<&str> = config.devices.iter().map(|d| d.name.as_str()).collect();
            Err(anyhow!(
                "no device matches {needle:?}. Known devices: {}",
                if known.is_empty() {
                    "none yet — run `homeremote discover --save`".to_string()
                } else {
                    known.join(", ")
                }
            ))
        }
        many => {
            let names: Vec<&str> = many.iter().map(|d| d.name.as_str()).collect();
            Err(anyhow!(
                "{needle:?} matches several devices: {}. Be more specific.",
                names.join(", ")
            ))
        }
    }
}

/// Parse command words, e.g. `["volume", "up"]` or `["input", "MPLAY"]`.
pub fn parse_command(words: &[String]) -> Result<Command> {
    let [verb, rest @ ..] = words else {
        bail!("expected a command, e.g. `power on` or `volume up`");
    };
    let arg = rest.first().map(|s| s.to_ascii_lowercase());

    match verb.to_ascii_lowercase().as_str() {
        "power" => Ok(Command::Power(match arg.as_deref() {
            Some("on") => PowerState::On,
            Some("off") => PowerState::Off,
            Some("toggle") => PowerState::Toggle,
            _ => bail!("power takes on, off, or toggle"),
        })),

        "volume" | "vol" => {
            let arg = arg.context("volume takes up, down, or a level from 0 to 1")?;
            Ok(Command::Volume(match arg.as_str() {
                "up" => VolumeCmd::Up,
                "down" => VolumeCmd::Down,
                level => VolumeCmd::Set(parse_level(level)?),
            }))
        }

        "mute" => Ok(Command::Volume(match arg.as_deref() {
            Some("on") | None => VolumeCmd::Mute(true),
            Some("off") => VolumeCmd::Mute(false),
            Some("toggle") => VolumeCmd::ToggleMute,
            _ => bail!("mute takes on, off, or toggle"),
        })),

        "input" => {
            let name = rest.first().context("input takes a name, e.g. MPLAY")?;
            Ok(Command::Input(name.clone()))
        }

        "nav" => Ok(Command::Nav(match arg.as_deref() {
            Some("up") => NavKey::Up,
            Some("down") => NavKey::Down,
            Some("left") => NavKey::Left,
            Some("right") => NavKey::Right,
            Some("select") | Some("ok") => NavKey::Select,
            Some("back") => NavKey::Back,
            Some("home") => NavKey::Home,
            Some("menu") => NavKey::Menu,
            _ => bail!("nav takes up, down, left, right, select, back, home, or menu"),
        })),

        "play" => Ok(Command::Transport(TransportKey::Play)),
        "pause" => Ok(Command::Transport(TransportKey::Pause)),
        "playpause" => Ok(Command::Transport(TransportKey::PlayPause)),
        "stop" => Ok(Command::Transport(TransportKey::Stop)),
        "next" => Ok(Command::Transport(TransportKey::Next)),
        "prev" | "previous" => Ok(Command::Transport(TransportKey::Previous)),

        // Everything after `raw` goes through untouched, so protocol strings
        // can be tried without a rebuild.
        "raw" => {
            if rest.is_empty() {
                bail!("raw takes a protocol string, e.g. `raw PSBAS UP`");
            }
            Ok(Command::Raw(rest.join(" ")))
        }

        other => bail!("unknown command {other:?}"),
    }
}

/// Accept both `0.4` and `40%` for a volume level.
fn parse_level(text: &str) -> Result<f32> {
    let (number, scale) = match text.strip_suffix('%') {
        Some(n) => (n, 100.0),
        None => (text, 1.0),
    };
    let value: f32 = number
        .parse()
        .with_context(|| format!("{text:?} is not a volume level"))?;
    let level = value / scale;
    if !(0.0..=1.0).contains(&level) {
        bail!("volume level must be between 0 and 1 (or 0% and 100%), got {text:?}");
    }
    Ok(level)
}

#[cfg(test)]
mod tests {
    use super::*;
    use homeremote::device::DeviceId;
    use homeremote::discovery::Vendor;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    fn config() -> Config {
        let device = |id: &str, name: &str, vendor| DeviceConfig {
            id: DeviceId::new(id),
            vendor,
            name: name.into(),
            addr: None,
            mac: None,
            credentials: None,
        };
        Config {
            devices: vec![
                device("denon:0005CD91E7A1", "Denon AVR-S720W", Vendor::Denon),
                device("samsung:098968", "[TV]Samsung LED55", Vendor::Samsung),
                device("appletv:582DF7CA", "Living Room", Vendor::AppleTv),
            ],
            ..Config::default()
        }
    }

    #[test]
    fn resolves_by_name_id_and_vendor() {
        let config = config();
        assert_eq!(resolve(&config, "denon").unwrap().name, "Denon AVR-S720W");
        assert_eq!(resolve(&config, "DENON").unwrap().name, "Denon AVR-S720W");
        assert_eq!(resolve(&config, "0005CD").unwrap().name, "Denon AVR-S720W");
        assert_eq!(resolve(&config, "living").unwrap().name, "Living Room");
    }

    #[test]
    fn refuses_to_guess_between_candidates() {
        // Both the TV and the Apple TV contain "tv"; picking one at random
        // could power off the wrong device.
        let err = resolve(&config(), "tv").unwrap_err().to_string();
        assert!(err.contains("matches several"), "got: {err}");
    }

    #[test]
    fn unknown_device_lists_what_is_known() {
        let err = resolve(&config(), "roku").unwrap_err().to_string();
        assert!(err.contains("Denon AVR-S720W"), "got: {err}");
    }

    #[test]
    fn parses_power_and_mute() {
        assert_eq!(
            parse_command(&words("power on")).unwrap(),
            Command::Power(PowerState::On)
        );
        assert_eq!(
            parse_command(&words("mute")).unwrap(),
            Command::Volume(VolumeCmd::Mute(true))
        );
        assert_eq!(
            parse_command(&words("mute toggle")).unwrap(),
            Command::Volume(VolumeCmd::ToggleMute)
        );
    }

    #[test]
    fn parses_volume_in_both_notations() {
        assert_eq!(
            parse_command(&words("volume up")).unwrap(),
            Command::Volume(VolumeCmd::Up)
        );
        assert_eq!(
            parse_command(&words("volume 0.4")).unwrap(),
            Command::Volume(VolumeCmd::Set(0.4))
        );
        assert_eq!(
            parse_command(&words("vol 40%")).unwrap(),
            Command::Volume(VolumeCmd::Set(0.4))
        );
    }

    #[test]
    fn rejects_out_of_range_volume() {
        // `volume 57` looks like receiver units and would otherwise be clamped
        // to full scale — deafening. Better to refuse it.
        assert!(parse_command(&words("volume 57")).is_err());
        assert!(parse_command(&words("volume -1")).is_err());
        assert!(parse_command(&words("volume loud")).is_err());
    }

    #[test]
    fn raw_keeps_the_whole_string() {
        assert_eq!(
            parse_command(&words("raw PSBAS UP")).unwrap(),
            Command::Raw("PSBAS UP".into())
        );
    }

    #[test]
    fn rejects_nonsense() {
        assert!(parse_command(&words("")).is_err());
        assert!(parse_command(&words("frobnicate")).is_err());
        assert!(parse_command(&words("input")).is_err());
        assert!(parse_command(&words("power sideways")).is_err());
    }
}
