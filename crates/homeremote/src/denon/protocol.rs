//! Encoding and decoding the Denon control protocol.
//!
//! Pure string handling, kept separate from the socket so the fiddly parts —
//! the half-step volume scale especially — are testable without a receiver.
//!
//! Commands are terminated with a bare `\r`, never `\n`.

use crate::device::{Command, NavKey, PowerState, TransportKey, VolumeCmd};
use crate::{Error, Result};

/// Command terminator.
pub const TERMINATOR: char = '\r';

/// Full-scale volume in the receiver's own units.
///
/// The AVR-S720W does not report `MVMAX` when asked, so this is the documented
/// protocol ceiling, used until the receiver volunteers otherwise.
pub const DEFAULT_MAX_VOLUME: f32 = 98.0;

/// The receiver was observed stepping by 0.5, not 1.0, in response to `MVUP`.
pub const VOLUME_STEP: f32 = 0.5;

/// A line received from the receiver.
///
/// Arrives both as a reply to a query and unsolicited, whenever anything
/// changes the receiver's state — including the physical remote. That is what
/// keeps the UI's volume honest.
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    /// Volume in receiver units (0..=max), not normalised.
    Volume(f32),
    /// The receiver announcing its own ceiling.
    MaxVolume(f32),
    Muted(bool),
    Power(bool),
    Input(String),
    /// A source's label, from `SSFUN ?`. Labels are user-editable on the
    /// receiver, so this is the name that actually appears on its display.
    InputName { code: String, label: String },
    /// Whether a source is enabled, from `SSSOD ?`. Disabled sources are hidden
    /// on the receiver and should be hidden in the remote too.
    InputEnabled { code: String, enabled: bool },
    /// Understood but not acted on, e.g. `SVOFF`.
    Other(String),
}

/// Parse one line from the receiver.
///
/// Returns `None` for empty lines.
pub fn parse_response(line: &str) -> Option<Response> {
    let line = line.trim_matches(|c| c == '\r' || c == '\n' || c == ' ');
    if line.is_empty() {
        return None;
    }

    // `MVMAX 98` must be tested before `MV`, or the ceiling is mistaken for the
    // current volume and the UI jumps to full scale.
    if let Some(rest) = line.strip_prefix("MVMAX") {
        return parse_volume_units(rest.trim()).map(Response::MaxVolume);
    }
    if let Some(rest) = line.strip_prefix("MV") {
        return Some(
            parse_volume_units(rest).map_or_else(|| Response::Other(line.to_string()), Response::Volume),
        );
    }
    if let Some(rest) = line.strip_prefix("MU") {
        return match rest {
            "ON" => Some(Response::Muted(true)),
            "OFF" => Some(Response::Muted(false)),
            _ => Some(Response::Other(line.to_string())),
        };
    }
    // `PW` is system power; `ZM` is the main zone. Either turning off means the
    // room is off as far as the remote is concerned.
    if let Some(rest) = line.strip_prefix("PW") {
        return match rest {
            "ON" => Some(Response::Power(true)),
            "STANDBY" => Some(Response::Power(false)),
            _ => Some(Response::Other(line.to_string())),
        };
    }
    if let Some(rest) = line.strip_prefix("ZM") {
        return match rest {
            "ON" => Some(Response::Power(true)),
            "OFF" => Some(Response::Power(false)),
            _ => Some(Response::Other(line.to_string())),
        };
    }
    // Source setup replies. These must be tested before the bare `SI` prefix
    // below, though they don't actually collide, and before falling through.
    if let Some(rest) = line.strip_prefix("SSFUN") {
        return Some(parse_source_setup(rest).map_or_else(
            || Response::Other(line.to_string()),
            |(code, label)| Response::InputName { code, label },
        ));
    }
    if let Some(rest) = line.strip_prefix("SSSOD") {
        return Some(parse_source_setup(rest).map_or_else(
            || Response::Other(line.to_string()),
            |(code, status)| Response::InputEnabled {
                code,
                enabled: status.eq_ignore_ascii_case("USE"),
            },
        ));
    }
    // `SV` is video-select, which shares the `S` prefix but is not an input.
    if let Some(rest) = line.strip_prefix("SI") {
        return Some(Response::Input(rest.to_string()));
    }
    Some(Response::Other(line.to_string()))
}

/// Split a source-setup payload into its code and value.
///
/// The code runs up to the first space and may itself contain a slash, as in
/// `SAT/CBL CBL/SAT` — code `SAT/CBL`, label `CBL/SAT`. Splitting anywhere but
/// the first space gets those wrong.
fn parse_source_setup(rest: &str) -> Option<(String, String)> {
    let (code, value) = rest.split_once(' ')?;
    let (code, value) = (code.trim(), value.trim());
    // `SSFUN END` terminates the listing rather than describing a source.
    if code.is_empty() || value.is_empty() || code.eq_ignore_ascii_case("END") {
        return None;
    }
    Some((code.to_string(), value.to_string()))
}

/// Parse the digits after `MV` into receiver units.
///
/// Two digits are whole units (`57` -> 57.0); three add a tenths digit
/// (`575` -> 57.5, `005` -> 0.5).
fn parse_volume_units(digits: &str) -> Option<f32> {
    if !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    match digits.len() {
        2 => digits.parse::<f32>().ok(),
        3 => {
            let whole: f32 = digits[..2].parse().ok()?;
            let tenths: f32 = digits[2..].parse().ok()?;
            Some(whole + tenths / 10.0)
        }
        _ => None,
    }
}

/// Format receiver units as the protocol's digits.
///
/// Rounds to the nearest half step, which is the finest the receiver accepts.
pub fn format_volume_units(units: f32, max: f32) -> String {
    let clamped = units.clamp(0.0, max);
    let stepped = (clamped / VOLUME_STEP).round() * VOLUME_STEP;
    let whole = stepped.trunc() as u32;
    if (stepped - stepped.trunc()).abs() < 0.01 {
        format!("{whole:02}")
    } else {
        format!("{whole:02}5")
    }
}

/// Map a normalised 0.0–1.0 level onto receiver units.
pub fn units_from_normalised(level: f32, max: f32) -> f32 {
    level.clamp(0.0, 1.0) * max
}

/// Map receiver units back onto 0.0–1.0.
pub fn normalised_from_units(units: f32, max: f32) -> f32 {
    if max <= 0.0 {
        return 0.0;
    }
    (units / max).clamp(0.0, 1.0)
}

/// Turn a [`Command`] into the protocol strings that carry it out.
///
/// Some commands need more than one line, so this returns a list.
pub fn encode(command: &Command, max_volume: f32) -> Result<Vec<String>> {
    let one = |s: String| Ok(vec![s]);
    match command {
        Command::Power(PowerState::On) => one("PWON".into()),
        Command::Power(PowerState::Off) => one("PWSTANDBY".into()),
        // The receiver has no toggle; the caller resolves it against known state.
        Command::Power(PowerState::Toggle) => Err(Error::Unsupported(
            "the receiver has no power toggle; send an explicit on or off",
        )),

        Command::Volume(VolumeCmd::Up) => one("MVUP".into()),
        Command::Volume(VolumeCmd::Down) => one("MVDOWN".into()),
        Command::Volume(VolumeCmd::Set(level)) => {
            let units = units_from_normalised(*level, max_volume);
            one(format!("MV{}", format_volume_units(units, max_volume)))
        }
        Command::Volume(VolumeCmd::Mute(true)) => one("MUON".into()),
        Command::Volume(VolumeCmd::Mute(false)) => one("MUOFF".into()),
        Command::Volume(VolumeCmd::ToggleMute) => Err(Error::Unsupported(
            "the receiver has no mute toggle; send an explicit mute or unmute",
        )),

        Command::Input(name) => {
            let name = name.trim().to_ascii_uppercase();
            if name.is_empty() {
                return Err(Error::Unsupported("input name cannot be empty"));
            }
            one(format!("SI{name}"))
        }

        Command::Nav(_) | Command::Transport(_) => Err(Error::Unsupported(
            "the receiver does not handle navigation or transport keys",
        )),
        Command::Text(_) => Err(Error::Unsupported("the receiver has no text input")),

        Command::Raw(raw) => {
            let raw = raw.trim();
            if raw.is_empty() {
                return Err(Error::Unsupported("raw command cannot be empty"));
            }
            one(raw.to_string())
        }
    }
}

/// The queries that establish full state on connect.
///
/// `SSFUN ?` and `SSSOD ?` enumerate the sources: which exist, what the owner
/// has renamed them to, and which are enabled. Without them the remote would
/// have to hard-code a guess at the input list.
pub const STATE_QUERIES: [&str; 7] = [
    "PW?", "ZM?", "MV?", "MU?", "SI?", "SSFUN ?", "SSSOD ?",
];

/// Friendly names for sources that `SSFUN ?` does not describe — the network
/// and tuner sources, which cannot be renamed on the receiver.
pub fn default_input_label(code: &str) -> &str {
    match code {
        "TUNER" => "Tuner",
        "BT" => "Bluetooth",
        "NET" => "HEOS Music",
        "USB/IPOD" => "USB / iPod",
        "USB" => "USB",
        "IPOD" => "iPod",
        "FAVORITES" => "Favourites",
        "IRADIO" => "Internet Radio",
        "SERVER" => "Media Server",
        "PANDORA" => "Pandora",
        "SIRIUSXM" => "SiriusXM",
        "SPOTIFY" => "Spotify",
        "PHONO" => "Phono",
        "CD" => "CD",
        other => other,
    }
}

/// Ignored by the [`Command::Nav`] arm above, listed so an exhaustive match on
/// the shared vocabulary stays honest if new keys are added.
#[allow(dead_code)]
fn _unsupported_keys(_: NavKey, _: TransportKey) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_whole_and_half_step_volumes() {
        // Both observed live: MVUP moved the receiver from MV57 to MV575.
        assert_eq!(parse_response("MV57"), Some(Response::Volume(57.0)));
        assert_eq!(parse_response("MV575"), Some(Response::Volume(57.5)));
        assert_eq!(parse_response("MV05"), Some(Response::Volume(5.0)));
        assert_eq!(parse_response("MV005"), Some(Response::Volume(0.5)));
        assert_eq!(parse_response("MV00"), Some(Response::Volume(0.0)));
    }

    #[test]
    fn max_volume_is_not_mistaken_for_current_volume() {
        // `MVMAX 98` shares the MV prefix. Reading it as a level would slam the
        // UI to full scale.
        assert_eq!(parse_response("MVMAX 98"), Some(Response::MaxVolume(98.0)));
        assert_eq!(parse_response("MVMAX 985"), Some(Response::MaxVolume(98.5)));
    }

    #[test]
    fn parses_power_from_both_system_and_zone() {
        assert_eq!(parse_response("PWON"), Some(Response::Power(true)));
        assert_eq!(parse_response("PWSTANDBY"), Some(Response::Power(false)));
        assert_eq!(parse_response("ZMON"), Some(Response::Power(true)));
        assert_eq!(parse_response("ZMOFF"), Some(Response::Power(false)));
    }

    #[test]
    fn parses_mute_and_input() {
        assert_eq!(parse_response("MUON"), Some(Response::Muted(true)));
        assert_eq!(parse_response("MUOFF"), Some(Response::Muted(false)));
        assert_eq!(parse_response("SIBD"), Some(Response::Input("BD".into())));
        assert_eq!(parse_response("SIMPLAY"), Some(Response::Input("MPLAY".into())));
    }

    #[test]
    fn parses_renamed_source_labels() {
        // Captured from the AVR-S720W: this unit has MPLAY renamed "Cameras".
        assert_eq!(
            parse_response("SSFUNMPLAY Cameras     "),
            Some(Response::InputName {
                code: "MPLAY".into(),
                label: "Cameras".into()
            })
        );
        assert_eq!(
            parse_response("SSFUNBD Blu-ray     "),
            Some(Response::InputName {
                code: "BD".into(),
                label: "Blu-ray".into()
            })
        );
    }

    #[test]
    fn a_source_code_may_contain_a_slash() {
        // `SSFUNSAT/CBL CBL/SAT` splits at the FIRST space, not the slash.
        assert_eq!(
            parse_response("SSFUNSAT/CBL CBL/SAT     "),
            Some(Response::InputName {
                code: "SAT/CBL".into(),
                label: "CBL/SAT".into()
            })
        );
        assert_eq!(
            parse_response("SSSODUSB/IPOD USE"),
            Some(Response::InputEnabled {
                code: "USB/IPOD".into(),
                enabled: true
            })
        );
    }

    #[test]
    fn parses_source_enablement() {
        assert_eq!(
            parse_response("SSSODDVD USE"),
            Some(Response::InputEnabled {
                code: "DVD".into(),
                enabled: true
            })
        );
        assert_eq!(
            parse_response("SSSODCD DEL"),
            Some(Response::InputEnabled {
                code: "CD".into(),
                enabled: false
            })
        );
    }

    #[test]
    fn source_listing_terminators_are_not_sources() {
        assert_eq!(
            parse_response("SSFUN END"),
            Some(Response::Other("SSFUN END".into()))
        );
        assert_eq!(
            parse_response("SSSOD END"),
            Some(Response::Other("SSSOD END".into()))
        );
    }

    #[test]
    fn network_sources_get_readable_default_labels() {
        // These never appear in SSFUN, so without a default the UI would show
        // raw protocol codes.
        assert_eq!(default_input_label("BT"), "Bluetooth");
        assert_eq!(default_input_label("USB/IPOD"), "USB / iPod");
        // An unknown code is shown as-is rather than hidden.
        assert_eq!(default_input_label("WEIRD"), "WEIRD");
    }

    #[test]
    fn video_select_is_not_an_input() {
        // `SI?` replies with both SIBD and SVOFF; only the first is the input.
        assert_eq!(parse_response("SVOFF"), Some(Response::Other("SVOFF".into())));
    }

    #[test]
    fn tolerates_line_endings_and_blank_lines() {
        assert_eq!(parse_response("MV57\r"), Some(Response::Volume(57.0)));
        assert_eq!(parse_response("  MV57  "), Some(Response::Volume(57.0)));
        assert_eq!(parse_response(""), None);
        assert_eq!(parse_response("\r"), None);
    }

    #[test]
    fn unknown_lines_survive_as_other() {
        // The receiver volunteers plenty we don't model; none of it may panic
        // or be silently read as a volume.
        assert_eq!(parse_response("PSBAS 50"), Some(Response::Other("PSBAS 50".into())));
        assert_eq!(parse_response("MVWEIRD"), Some(Response::Other("MVWEIRD".into())));
    }

    #[test]
    fn formats_volume_at_half_step_resolution() {
        assert_eq!(format_volume_units(57.0, 98.0), "57");
        assert_eq!(format_volume_units(57.5, 98.0), "575");
        assert_eq!(format_volume_units(5.0, 98.0), "05");
        assert_eq!(format_volume_units(0.5, 98.0), "005");
        assert_eq!(format_volume_units(0.0, 98.0), "00");
    }

    #[test]
    fn formatting_rounds_to_the_nearest_step() {
        assert_eq!(format_volume_units(57.3, 98.0), "575");
        assert_eq!(format_volume_units(57.1, 98.0), "57");
        assert_eq!(format_volume_units(57.8, 98.0), "58");
    }

    #[test]
    fn formatting_clamps_to_the_ceiling() {
        // Nothing may talk the receiver past its own maximum.
        assert_eq!(format_volume_units(200.0, 98.0), "98");
        assert_eq!(format_volume_units(-10.0, 98.0), "00");
    }

    #[test]
    fn volume_round_trips_through_the_wire_format() {
        let mut units = 0.0;
        while units <= 98.0 {
            let encoded = format_volume_units(units, 98.0);
            let decoded = parse_response(&format!("MV{encoded}")).unwrap();
            assert_eq!(
                decoded,
                Response::Volume(units),
                "{units} did not survive as {encoded}"
            );
            units += VOLUME_STEP;
        }
    }

    #[test]
    fn normalisation_maps_onto_the_full_scale() {
        assert_eq!(units_from_normalised(0.0, 98.0), 0.0);
        assert_eq!(units_from_normalised(1.0, 98.0), 98.0);
        assert_eq!(units_from_normalised(0.5, 98.0), 49.0);
        // Out-of-range input must not produce out-of-range volume.
        assert_eq!(units_from_normalised(5.0, 98.0), 98.0);
        assert_eq!(units_from_normalised(-1.0, 98.0), 0.0);
    }

    #[test]
    fn normalisation_round_trips() {
        for level in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
            let units = units_from_normalised(level, 98.0);
            assert!((normalised_from_units(units, 98.0) - level).abs() < 0.001);
        }
    }

    #[test]
    fn encodes_the_common_commands() {
        let enc = |c: Command| encode(&c, 98.0).unwrap();
        assert_eq!(enc(Command::Power(PowerState::On)), ["PWON"]);
        assert_eq!(enc(Command::Power(PowerState::Off)), ["PWSTANDBY"]);
        assert_eq!(enc(Command::Volume(VolumeCmd::Up)), ["MVUP"]);
        assert_eq!(enc(Command::Volume(VolumeCmd::Down)), ["MVDOWN"]);
        assert_eq!(enc(Command::Volume(VolumeCmd::Mute(true))), ["MUON"]);
        assert_eq!(enc(Command::Volume(VolumeCmd::Set(0.5))), ["MV49"]);
        assert_eq!(enc(Command::Input("mplay".into())), ["SIMPLAY"]);
        assert_eq!(enc(Command::Raw("PSBAS UP".into())), ["PSBAS UP"]);
    }

    #[test]
    fn rejects_what_the_receiver_cannot_do() {
        // A toggle needs state the protocol layer doesn't have; failing loudly
        // beats guessing and turning the room off.
        assert!(encode(&Command::Power(PowerState::Toggle), 98.0).is_err());
        assert!(encode(&Command::Volume(VolumeCmd::ToggleMute), 98.0).is_err());
        assert!(encode(&Command::Nav(NavKey::Up), 98.0).is_err());
        assert!(encode(&Command::Transport(TransportKey::Play), 98.0).is_err());
        assert!(encode(&Command::Input("  ".into()), 98.0).is_err());
        assert!(encode(&Command::Raw("".into()), 98.0).is_err());
    }
}
