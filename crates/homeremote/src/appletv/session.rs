//! Pairing and controlling an Apple TV over Companion.

use std::net::{IpAddr, SocketAddr};

use crate::appletv::frame::FrameType;
use crate::appletv::link::Link;
use crate::appletv::opack::{Value, dict};
use crate::appletv::pairing::{Credentials, PairSetup, PairVerify};
use crate::appletv::rti::{self, TextField};
use crate::appletv::tlv8::Tlv;
use crate::appletv::{DEFAULT_PORT, frame};
use crate::device::{NavKey, TransportKey};
use crate::{Error, Result};

/// HID button codes.
///
/// Values are the device's own; they are not arbitrary and must not be
/// renumbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum HidCommand {
    Up = 1,
    Down = 2,
    Left = 3,
    Right = 4,
    Menu = 5,
    Select = 6,
    Home = 7,
    VolumeUp = 8,
    VolumeDown = 9,
    Siri = 10,
    Screensaver = 11,
    Sleep = 12,
    Wake = 13,
    PlayPause = 14,
    ChannelIncrement = 15,
    ChannelDecrement = 16,
    Guide = 17,
    PageUp = 18,
    PageDown = 19,
}

impl HidCommand {
    pub fn for_nav(key: NavKey) -> Self {
        match key {
            NavKey::Up => Self::Up,
            NavKey::Down => Self::Down,
            NavKey::Left => Self::Left,
            NavKey::Right => Self::Right,
            NavKey::Select => Self::Select,
            // tvOS has no separate back and menu — Menu *is* back. Both map
            // here, so the remote only offers one of them.
            NavKey::Back | NavKey::Menu => Self::Menu,
            NavKey::Home => Self::Home,
        }
    }

    /// Transport keys the Apple TV exposes over HID.
    ///
    /// It has one play/pause toggle rather than separate keys, and no track
    /// skip on this interface.
    pub fn for_transport(key: TransportKey) -> Result<Self> {
        match key {
            TransportKey::Play | TransportKey::Pause | TransportKey::PlayPause => {
                Ok(Self::PlayPause)
            }
            TransportKey::Next => Ok(Self::ChannelIncrement),
            TransportKey::Previous => Ok(Self::ChannelDecrement),
            TransportKey::Stop => Err(Error::Unsupported(
                "the Apple TV has no stop button; use pause",
            )),
        }
    }
}

/// A pairing that has started and is waiting for the PIN on screen.
///
/// Holds the connection open, because the device ties the PIN to this session.
pub struct PendingPairing {
    link: Link,
    setup: PairSetup,
}

impl PendingPairing {
    /// Complete pairing with the PIN shown on the TV.
    pub async fn finish(mut self, pin: &str) -> Result<Credentials> {
        let pin = pin.trim();
        if pin.is_empty() || !pin.chars().all(|c| c.is_ascii_digit()) {
            return Err(Error::Protocol(
                "the PIN shown on the Apple TV is digits only".into(),
            ));
        }

        // M2 arrived when pairing began; prove we know the PIN.
        let m2 = self.setup.pending_m2()?;
        let m3 = self.setup.prove_pin(&m2, pin)?;
        let m4 = exchange_pairing(
            &mut self.link,
            FrameType::PsNext,
            m3,
            &[("_pwTy", Value::Int(1))],
            "M3 (proving the PIN)",
        )
        .await?;

        let m5 = self.setup.exchange_identity(&m4)?;
        let m6 = exchange_pairing(
            &mut self.link,
            FrameType::PsNext,
            m5,
            &[("_pwTy", Value::Int(1))],
            "M5 (exchanging identities)",
        )
        .await?;

        self.setup.finish(&m6)
    }
}

/// Send a TLV as pairing data and decode the TLV that comes back.
async fn exchange_pairing(
    link: &mut Link,
    frame_type: FrameType,
    tlv: Tlv,
    extra: &[(&str, Value)],
    step: &str,
) -> Result<Tlv> {
    let xid = link.take_xid();
    let mut fields: Vec<(&str, Value)> = extra.to_vec();
    fields.push(("_x", Value::Int(xid)));
    let message = frame::pairing_message(tlv.encode(), &fields);

    tracing::debug!(step, ?frame_type, "pairing ->");
    let reply = link.exchange(frame_type, &message).await.map_err(|e| {
        // Naming the step turns "early eof" into something diagnosable.
        Error::Protocol(format!("{step} failed: {e}"))
    })?;
    tracing::debug!(step, "pairing <- ok");

    let data = frame::pairing_data(&reply)?;
    let tlv = Tlv::decode(data)?;
    tracing::debug!(step, tlv = %tlv.describe(), "pairing <- fields");
    if let Some(error) = tlv.error() {
        let wait = tlv
            .get(crate::appletv::tlv8::Tag::RetryDelay)
            .and_then(|v| v.first())
            .map(|secs| format!(" — wait {secs}s before trying again"))
            .unwrap_or_default();
        return Err(Error::Protocol(format!(
            "device refused pairing at {step}: {error}{wait}"
        )));
    }
    Ok(tlv)
}

/// Begin pairing. The Apple TV puts a four-digit PIN on screen.
pub async fn begin_pairing(addr: IpAddr, port: Option<u16>) -> Result<PendingPairing> {
    let target = SocketAddr::new(addr, port.unwrap_or(DEFAULT_PORT));
    let mut link = Link::connect(target).await?;

    let client_id = new_client_id();
    let mut setup = PairSetup::new(random_seed(), client_id);

    // `_pwTy: 1` asks for the on-screen PIN flow.
    let m2 = exchange_pairing(
        &mut link,
        FrameType::PsStart,
        setup.start(),
        &[("_pwTy", Value::Int(1))],
        "M1 (starting pairing)",
    )
    .await?;
    setup.accept_m2(m2);

    Ok(PendingPairing { link, setup })
}

/// Connect and bring up an encrypted, session-started link.
pub async fn open_session(
    addr: IpAddr,
    port: Option<u16>,
    credentials: &Credentials,
) -> Result<Link> {
    let target = SocketAddr::new(addr, port.unwrap_or(DEFAULT_PORT));
    let mut link = Link::connect(target).await?;

    let mut verify = PairVerify::new(random_seed(), credentials.clone());
    let m2 = exchange_pairing(
        &mut link,
        FrameType::PvStart,
        verify.start(),
        &[("_auTy", Value::Int(4))],
        "verify M1",
    )
    .await?;

    let m3 = verify.respond(&m2)?;
    exchange_pairing(&mut link, FrameType::PvNext, m3, &[], "verify M3").await?;

    let (out_key, in_key) = verify.session_keys()?;
    link.enable_encryption(out_key, in_key);

    // Introduce ourselves as an event rather than a request. This device never
    // answers _systemInfo, so asking and waiting cost the full I/O timeout on
    // every reconnect — which surfaced as the remote freezing for seconds and
    // then firing every queued press at once.
    let client_id = String::from_utf8_lossy(&credentials.client_id).to_string();
    link.send_event(
        "_systemInfo",
        dict([
            ("_bf", Value::Int(0)),
            ("_cf", Value::Int(512)),
            ("_clFl", Value::Int(128)),
            ("_i", Value::from("homeremote")),
            ("_idsID", Value::from(client_id)),
            ("_pubID", Value::from("homeremote")),
            ("_sf", Value::Int(256)),
            ("_sv", Value::from("170.18")),
            ("model", Value::from("homeremote")),
            ("name", Value::from("Home Remote")),
        ]),
    )
    .await?;

    let local_sid: i64 = i64::from(rand::random::<u32>());
    link.command(
        "_sessionStart",
        dict([
            ("_srvT", Value::from("com.apple.tvremoteservices")),
            ("_sid", Value::Int(local_sid)),
        ]),
    )
    .await?;

    Ok(link)
}

/// Press and release a button.
///
/// A press is two HID events; sending only the down event leaves the Apple TV
/// believing the button is still held.
///
/// Both frames go out before either reply is collected. Waiting for the first
/// answer before sending the second doubles the cost of every press, which is
/// what a held button runs into first.
pub async fn press(link: &mut Link, command: HidCommand) -> Result<()> {
    for state in [1i64, 2] {
        link.send_command(
            "_hidC",
            dict([("_hBtS", Value::Int(state)), ("_hidC", Value::Int(command as i64))]),
        )
        .await?;
    }
    for _ in 0..2 {
        link.await_reply().await?;
    }
    Ok(())
}

/// Ask about text input, and register for `_tiStarted`/`_tiStopped` events.
///
/// Returns the field focused right now, if any. Those events are meant to
/// announce changes, but the Living Room Apple TV (tvOS, 2026) was never seen
/// to send one, so this answer is what the remote actually relies on.
///
/// Safe to repeat on one session without a `_tiStop` in between, and each
/// answer is current; stopping first would flash on the TV.
pub async fn start_text_input(link: &mut Link) -> Result<Option<TextField>> {
    let reply = link.command("_tiStart", Value::Dict(Default::default())).await?;
    text_field(&reply)
}

/// The focused field described by a `_tiStart` reply or `_tiStarted` event.
pub fn text_field(content: &Value) -> Result<Option<TextField>> {
    match content.get("_tiD").and_then(Value::as_bytes) {
        Some(archive) => rti::read_field(archive).map(Some),
        None => Ok(None),
    }
}

/// Make the field hold `text`.
///
/// `from` is what the field holds, if known. The device's own report can't
/// be trusted for that — YouTube's search field reads as empty when you come
/// back to it with a term still in it — so until something has been typed
/// here, pass `None`: the field is then cleared before typing, rather than
/// typing onto whatever is left in it.
///
/// Once the contents are known, changes go as an edit at the end — backspace
/// over what differs, then type the rest. Clearing makes the Apple TV flash
/// its keyboard hint, so it is kept to that first change.
///
/// Sent straight to the field's session, without restarting text input to
/// look it up again, which flashes too.
pub async fn set_text(
    link: &mut Link,
    session: &[u8],
    from: Option<&str>,
    text: &str,
) -> Result<()> {
    let (deletions, insertion) = match from {
        Some(from) => edit_between(from, text),
        None => {
            send_text_operations(link, rti::clear(session)?).await?;
            (0, text)
        }
    };
    if deletions == 0 && insertion.is_empty() {
        return Ok(());
    }
    send_text_operations(link, rti::edit(session, deletions, insertion)?).await
}

/// How many characters to delete from the end of `from`, and what to type
/// after, to arrive at `to`.
fn edit_between<'a>(from: &str, to: &'a str) -> (usize, &'a str) {
    let shared = from
        .char_indices()
        .zip(to.chars())
        .find(|((_, a), b)| a != b)
        .map_or(from.len().min(to.len()), |((at, _), _)| at);
    // Everything before `shared` is identical, so it is a boundary in both.
    (from[shared..].chars().count(), &to[shared..])
}

async fn send_text_operations(link: &mut Link, archive: Vec<u8>) -> Result<()> {
    link.send_event(
        "_tiC",
        dict([("_tiV", Value::Int(1)), ("_tiD", Value::Bytes(archive))]),
    )
    .await
}

fn random_seed() -> [u8; 32] {
    let mut seed = [0u8; 32];
    for chunk in seed.chunks_mut(4) {
        chunk.copy_from_slice(&rand::random::<u32>().to_le_bytes());
    }
    seed
}

/// A stable-looking identifier for this remote, in the shape the device expects.
fn new_client_id() -> String {
    let bytes: [u8; 16] = std::array::from_fn(|_| rand::random::<u8>());
    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02X}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].concat(),
        hex[4..6].concat(),
        hex[6..8].concat(),
        hex[8..10].concat(),
        hex[10..16].concat()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nav_keys_map_onto_hid_codes() {
        assert_eq!(HidCommand::for_nav(NavKey::Up), HidCommand::Up);
        assert_eq!(HidCommand::for_nav(NavKey::Select), HidCommand::Select);
        assert_eq!(HidCommand::for_nav(NavKey::Home), HidCommand::Home);
        // The Apple TV has no distinct back button.
        assert_eq!(HidCommand::for_nav(NavKey::Back), HidCommand::Menu);
        assert_eq!(HidCommand::for_nav(NavKey::Menu), HidCommand::Menu);
    }

    #[test]
    fn transport_keys_collapse_onto_play_pause() {
        // The device offers one toggle, not separate play and pause.
        for key in [TransportKey::Play, TransportKey::Pause, TransportKey::PlayPause] {
            assert_eq!(HidCommand::for_transport(key).unwrap(), HidCommand::PlayPause);
        }
        assert!(HidCommand::for_transport(TransportKey::Stop).is_err());
    }

    #[test]
    fn hid_codes_match_the_devices_numbering() {
        // These are the device's values; renumbering them silently sends the
        // wrong button.
        assert_eq!(HidCommand::Up as i64, 1);
        assert_eq!(HidCommand::Select as i64, 6);
        assert_eq!(HidCommand::Home as i64, 7);
        assert_eq!(HidCommand::PlayPause as i64, 14);
    }

    #[test]
    fn edits_type_only_what_changed_at_the_end() {
        assert_eq!(edit_between("star", "star wars"), (0, " wars"));
        assert_eq!(edit_between("star wars", "star war"), (1, ""));
        assert_eq!(edit_between("star wars", "star trek"), (4, "trek"));
        assert_eq!(edit_between("star", "star"), (0, ""));
        assert_eq!(edit_between("star", ""), (4, ""));
        assert_eq!(edit_between("", "hi"), (0, "hi"));
        assert_eq!(edit_between("café", "cafe"), (1, "e"));
        assert_eq!(edit_between("naïve", "naïvety"), (0, "ty"));
    }

    #[test]
    fn client_ids_look_like_uuids_and_differ() {
        let id = new_client_id();
        assert_eq!(id.len(), 36);
        assert_eq!(id.matches('-').count(), 4);
        assert_ne!(id, new_client_id());
    }

    #[test]
    fn seeds_are_not_constant() {
        assert_ne!(random_seed(), random_seed());
    }
}
