//! The owned connection to the receiver.
//!
//! The AVR accepts exactly one client on its control port — a second `connect`
//! is refused outright with `ECONNREFUSED` (verified against an AVR-S720W). So
//! this task takes the socket once and holds it for the life of the daemon,
//! and everything else talks to it through a channel.
//!
//! Holding the socket open is not just about exclusivity: the receiver
//! volunteers every state change on it, including ones made with the physical
//! remote. That stream is what keeps the UI's volume from drifting.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::denon::CONTROL_PORT;
use crate::denon::protocol::{self, DEFAULT_MAX_VOLUME, Response, STATE_QUERIES, TERMINATOR};
use crate::device::{
    Capabilities, Command, Device, DeviceEvent, DeviceId, DeviceKind, DeviceState, InputOption,
    PowerState, VolumeCmd,
};
use crate::discovery::Discovered;
use crate::{Error, Result};

/// The receiver drops commands sent faster than this.
const MIN_COMMAND_GAP: Duration = Duration::from_millis(50);

/// A TCP connect to a sleeping receiver should fail fast, not hang for the
/// OS default.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

const RECONNECT_DELAY_MIN: Duration = Duration::from_secs(1);
const RECONNECT_DELAY_MAX: Duration = Duration::from_secs(30);

/// How many commands may queue before callers are told the receiver is busy.
const COMMAND_QUEUE: usize = 32;

/// Enough headroom that a slow subscriber misses nothing during a volume sweep.
const EVENT_QUEUE: usize = 64;

struct Request {
    lines: Vec<String>,
    ack: oneshot::Sender<Result<()>>,
}

#[derive(Debug, Default)]
struct Sources {
    /// Owner-assigned labels from `SSFUN ?`, e.g. MPLAY -> "Cameras".
    labels: HashMap<String, String>,
    /// Codes reported enabled by `SSSOD ?`, in the order the receiver listed
    /// them — that order matches its own on-screen menu.
    enabled: Vec<String>,
}

impl Sources {
    /// Compose the list the remote should show.
    ///
    /// Physical inputs — the ones `SSFUN ?` names — come first, since those are
    /// what anyone is actually reaching for. Tuner and streaming sources follow.
    fn options(&self) -> Vec<InputOption> {
        let mut named = Vec::new();
        let mut rest = Vec::new();
        for code in &self.enabled {
            match self.labels.get(code) {
                Some(label) => named.push(InputOption {
                    code: code.clone(),
                    label: label.clone(),
                }),
                None => rest.push(InputOption {
                    code: code.clone(),
                    label: protocol::default_input_label(code).to_string(),
                }),
            }
        }
        named.extend(rest);
        named
    }
}

#[derive(Debug)]
struct Inner {
    state: DeviceState,
    /// Updated if the receiver ever volunteers `MVMAX`. The AVR-S720W does not.
    max_volume: f32,
    sources: Sources,
}

/// A Denon receiver, controlled over its telnet port.
pub struct DenonReceiver {
    id: DeviceId,
    name: String,
    addr: IpAddr,
    commands: mpsc::Sender<Request>,
    events: broadcast::Sender<DeviceEvent>,
    inner: Arc<Mutex<Inner>>,
}

impl DenonReceiver {
    /// Take the receiver's control socket and start serving it.
    ///
    /// Returns immediately; the connection is established in the background and
    /// re-established on loss, so a receiver that is unplugged and plugged back
    /// in recovers without restarting the daemon.
    pub fn connect(found: &Discovered) -> Self {
        let (tx, rx) = mpsc::channel(COMMAND_QUEUE);
        let (events, _) = broadcast::channel(EVENT_QUEUE);
        let inner = Arc::new(Mutex::new(Inner {
            state: DeviceState::default(),
            max_volume: DEFAULT_MAX_VOLUME,
            sources: Sources::default(),
        }));

        let receiver = Self {
            id: found.id.clone(),
            name: found.name.clone(),
            addr: found.addr,
            commands: tx,
            events: events.clone(),
            inner: Arc::clone(&inner),
        };

        tokio::spawn(supervise(found.addr, rx, events, inner));
        receiver
    }

    pub fn addr(&self) -> IpAddr {
        self.addr
    }

    /// Resolve commands the protocol has no literal form for.
    ///
    /// The receiver offers no power or mute toggle, so they are turned into an
    /// explicit on or off against known state. If the state is unknown, this
    /// says so rather than guessing — guessing wrong turns the room off.
    fn resolve(&self, command: Command) -> Result<Command> {
        let state = self.inner.lock().unwrap().state.clone();
        match command {
            Command::Power(PowerState::Toggle) => match state.power {
                Some(true) => Ok(Command::Power(PowerState::Off)),
                Some(false) => Ok(Command::Power(PowerState::On)),
                None => Err(Error::Unsupported(
                    "cannot toggle power: the receiver's state is not known yet",
                )),
            },
            Command::Volume(VolumeCmd::ToggleMute) => match state.muted {
                Some(muted) => Ok(Command::Volume(VolumeCmd::Mute(!muted))),
                None => Err(Error::Unsupported(
                    "cannot toggle mute: the receiver's state is not known yet",
                )),
            },
            other => Ok(other),
        }
    }
}

#[async_trait::async_trait]
impl Device for DenonReceiver {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn kind(&self) -> DeviceKind {
        DeviceKind::Receiver
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            power: true,
            volume: true,
            input: true,
            // The receiver has no on-screen navigation worth exposing.
            nav: false,
            transport: false,
        }
    }

    async fn send(&self, command: Command) -> Result<()> {
        let command = self.resolve(command)?;
        let max_volume = self.inner.lock().unwrap().max_volume;
        let lines = protocol::encode(&command, max_volume)?;

        let (ack, acked) = oneshot::channel();
        self.commands
            .try_send(Request { lines, ack })
            .map_err(|_| Error::Unreachable(self.addr))?;
        acked.await.map_err(|_| Error::Unreachable(self.addr))?
    }

    async fn state(&self) -> DeviceState {
        self.inner.lock().unwrap().state.clone()
    }

    async fn inputs(&self) -> Vec<InputOption> {
        self.inner.lock().unwrap().sources.options()
    }

    fn subscribe(&self) -> broadcast::Receiver<DeviceEvent> {
        self.events.subscribe()
    }
}

/// Keep a connection up for as long as the daemon runs.
async fn supervise(
    addr: IpAddr,
    mut commands: mpsc::Receiver<Request>,
    events: broadcast::Sender<DeviceEvent>,
    inner: Arc<Mutex<Inner>>,
) {
    let target = SocketAddr::new(addr, CONTROL_PORT);
    let mut delay = RECONNECT_DELAY_MIN;

    loop {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(target)).await {
            Ok(Ok(stream)) => {
                tracing::info!(%addr, "connected to the receiver");
                delay = RECONNECT_DELAY_MIN;
                set_reachable(&inner, &events, true);

                if let Err(e) = serve(stream, &mut commands, &events, &inner).await {
                    tracing::warn!(%addr, error = %e, "receiver connection dropped");
                }
                set_reachable(&inner, &events, false);

                // The command channel closed: the receiver was dropped, so stop.
                if commands.is_closed() {
                    return;
                }
            }
            Ok(Err(e)) => {
                // ECONNREFUSED here almost always means something else already
                // holds the single control connection.
                if e.kind() == std::io::ErrorKind::ConnectionRefused {
                    tracing::warn!(
                        %addr,
                        "receiver refused the connection — it allows only one at a \
                         time, and something already has it. Usually that is a \
                         `homeremote serve` already running; it can also be an old \
                         Harmony hub or Home Assistant."
                    );
                } else {
                    tracing::debug!(%addr, error = %e, "could not reach the receiver");
                }
                set_reachable(&inner, &events, false);
            }
            Err(_) => {
                tracing::debug!(%addr, "timed out connecting to the receiver");
                set_reachable(&inner, &events, false);
            }
        }

        // Reject anything queued while we wait, so callers get a prompt failure
        // instead of a command that lands minutes later.
        if !reject_for(&mut commands, delay, addr).await {
            return;
        }
        delay = (delay * 2).min(RECONNECT_DELAY_MAX);
    }
}

/// Pump one connection until it fails.
async fn serve(
    stream: TcpStream,
    commands: &mut mpsc::Receiver<Request>,
    events: &broadcast::Sender<DeviceEvent>,
    inner: &Arc<Mutex<Inner>>,
) -> std::io::Result<()> {
    // Control messages are tiny and latency matters more than packing them.
    let _ = stream.set_nodelay(true);
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).split(TERMINATOR as u8);
    let mut last_write = tokio::time::Instant::now() - MIN_COMMAND_GAP;

    // Ask for everything up front: the receiver reports state on change, so
    // without this the UI starts blank until someone touches a control.
    for query in STATE_QUERIES {
        write_line(&mut write_half, query, &mut last_write).await?;
    }

    loop {
        tokio::select! {
            // Biased so state from the receiver is applied before we act on a
            // command that might depend on it.
            biased;

            line = lines.next_segment() => {
                let Some(line) = line? else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "receiver closed the connection",
                    ));
                };
                let text = String::from_utf8_lossy(&line);
                apply(&text, events, inner);
            }

            request = commands.recv() => {
                let Some(request) = request else {
                    return Ok(()); // the receiver handle was dropped
                };
                let mut result = Ok(());
                for line in &request.lines {
                    if let Err(e) = write_line(&mut write_half, line, &mut last_write).await {
                        result = Err(Error::Io(e));
                        break;
                    }
                }
                let failed = result.is_err();
                let _ = request.ack.send(result);
                if failed {
                    return Err(std::io::Error::other("write to the receiver failed"));
                }
            }
        }
    }
}

/// Write one command, never faster than the receiver can take them.
async fn write_line(
    write_half: &mut tokio::net::tcp::OwnedWriteHalf,
    line: &str,
    last_write: &mut tokio::time::Instant,
) -> std::io::Result<()> {
    let since = last_write.elapsed();
    if since < MIN_COMMAND_GAP {
        tokio::time::sleep(MIN_COMMAND_GAP - since).await;
    }
    tracing::debug!(command = line, "-> receiver");
    write_half.write_all(line.as_bytes()).await?;
    write_half.write_all(&[TERMINATOR as u8]).await?;
    write_half.flush().await?;
    *last_write = tokio::time::Instant::now();
    Ok(())
}

/// Fold one line from the receiver into state and publish what changed.
fn apply(line: &str, events: &broadcast::Sender<DeviceEvent>, inner: &Arc<Mutex<Inner>>) {
    let Some(response) = protocol::parse_response(line) else {
        return;
    };
    tracing::debug!(?response, "<- receiver");

    let event = {
        let mut guard = inner.lock().unwrap();
        match response {
            Response::MaxVolume(max) => {
                guard.max_volume = max;
                // Rescale the level we're holding against the new ceiling.
                None
            }
            Response::Volume(units) => {
                let level = protocol::normalised_from_units(units, guard.max_volume);
                guard.state.volume = Some(level);
                Some(DeviceEvent::Volume(level))
            }
            Response::Muted(muted) => {
                guard.state.muted = Some(muted);
                Some(DeviceEvent::Muted(muted))
            }
            Response::Power(on) => {
                guard.state.power = Some(on);
                Some(DeviceEvent::Power(on))
            }
            Response::Input(input) => {
                guard.state.input = Some(input.clone());
                Some(DeviceEvent::Input(input))
            }
            Response::InputName { code, label } => {
                guard.sources.labels.insert(code, label);
                None
            }
            Response::InputEnabled { code, enabled } => {
                guard.sources.enabled.retain(|c| c != &code);
                if enabled {
                    guard.sources.enabled.push(code);
                }
                None
            }
            Response::Other(_) => None,
        }
    };

    // A send with no subscribers is not an error; nobody may be watching yet.
    if let Some(event) = event {
        let _ = events.send(event);
    }
}

fn set_reachable(inner: &Arc<Mutex<Inner>>, events: &broadcast::Sender<DeviceEvent>, up: bool) {
    let changed = {
        let mut guard = inner.lock().unwrap();
        let changed = guard.state.reachable != up;
        guard.state.reachable = up;
        if !up {
            // State learned while connected is stale the moment we lose the
            // socket; reporting it as current would be a lie.
            guard.state.power = None;
            guard.state.volume = None;
            guard.state.muted = None;
            guard.state.input = None;
        }
        changed
    };
    if changed {
        let _ = events.send(DeviceEvent::Reachability(up));
    }
}

/// Fail queued commands for `delay`. Returns false once the channel is closed.
async fn reject_for(commands: &mut mpsc::Receiver<Request>, delay: Duration, addr: IpAddr) -> bool {
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return true,
            request = commands.recv() => match request {
                Some(request) => {
                    let _ = request.ack.send(Err(Error::Unreachable(addr)));
                }
                None => return false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::Vendor;
    use std::net::Ipv4Addr;

    fn shared() -> (Arc<Mutex<Inner>>, broadcast::Sender<DeviceEvent>) {
        let (events, _keep) = broadcast::channel(16);
        // Hold a subscriber so sends aren't dropped for want of listeners.
        std::mem::forget(_keep);
        (
            Arc::new(Mutex::new(Inner {
                state: DeviceState::default(),
                max_volume: DEFAULT_MAX_VOLUME,
                sources: Sources::default(),
            })),
            events,
        )
    }

    #[test]
    fn applying_lines_builds_up_state() {
        let (inner, events) = shared();
        for line in ["PWON", "MV57", "MUOFF", "SIBD"] {
            apply(line, &events, &inner);
        }
        let state = inner.lock().unwrap().state.clone();
        assert_eq!(state.power, Some(true));
        assert_eq!(state.muted, Some(false));
        assert_eq!(state.input.as_deref(), Some("BD"));
        // 57 of 98 full scale.
        assert!((state.volume.unwrap() - 57.0 / 98.0).abs() < 0.001);
    }

    #[test]
    fn max_volume_rescales_later_readings_and_is_never_a_level() {
        let (inner, events) = shared();
        apply("MVMAX 60", &events, &inner);
        assert_eq!(
            inner.lock().unwrap().state.volume,
            None,
            "the ceiling is not a volume reading"
        );
        apply("MV30", &events, &inner);
        let volume = inner.lock().unwrap().state.volume.unwrap();
        assert!((volume - 0.5).abs() < 0.001, "30 of 60 should be half scale");
    }

    #[test]
    fn unrecognised_lines_change_nothing() {
        let (inner, events) = shared();
        apply("MV57", &events, &inner);
        apply("PSBAS 50", &events, &inner);
        apply("SVOFF", &events, &inner);
        let state = inner.lock().unwrap().state.clone();
        assert!(state.volume.is_some());
        assert_eq!(state.input, None, "SVOFF is video-select, not an input");
    }

    #[tokio::test]
    async fn state_changes_are_published() {
        let (inner, events) = shared();
        let mut subscriber = events.subscribe();
        apply("MV57", &events, &inner);
        assert!(matches!(
            subscriber.recv().await.unwrap(),
            DeviceEvent::Volume(_)
        ));
    }

    /// The exact reply set captured from the AVR-S720W.
    const SOURCE_REPLIES: &[&str] = &[
        "SSFUNDVD DVD/Blu-ray ",
        "SSFUNBD Blu-ray     ",
        "SSFUNTV TV Audio    ",
        "SSFUNSAT/CBL CBL/SAT     ",
        "SSFUNMPLAY Cameras     ",
        "SSFUNGAME Game        ",
        "SSFUNAUX1 AUX         ",
        "SSSODDVD USE",
        "SSSODBD USE",
        "SSSODTV USE",
        "SSSODSAT/CBL USE",
        "SSSODMPLAY USE",
        "SSSODBT USE",
        "SSSODGAME USE",
        "SSSODAUX1 USE",
        "SSSODTUNER USE",
        "SSSODUSB/IPOD USE",
    ];

    fn sources_from_replies() -> Vec<crate::device::InputOption> {
        let (inner, events) = shared();
        for line in SOURCE_REPLIES {
            apply(line, &events, &inner);
        }
        inner.lock().unwrap().sources.options()
    }

    #[test]
    fn inputs_use_the_names_set_on_the_receiver() {
        // This unit has MPLAY renamed "Cameras". Showing "MPLAY" instead would
        // mean the remote disagrees with the receiver's own display.
        let options = sources_from_replies();
        let cameras = options.iter().find(|o| o.code == "MPLAY").unwrap();
        assert_eq!(cameras.label, "Cameras");
    }

    #[test]
    fn physical_inputs_are_listed_before_streaming_sources() {
        // Someone reaching for the remote wants HDMI, not Bluetooth.
        let options = sources_from_replies();
        let position = |code: &str| options.iter().position(|o| o.code == code).unwrap();
        assert!(position("MPLAY") < position("BT"));
        assert!(position("SAT/CBL") < position("TUNER"));
    }

    #[test]
    fn sources_without_a_custom_name_still_read_well() {
        let options = sources_from_replies();
        let bluetooth = options.iter().find(|o| o.code == "BT").unwrap();
        assert_eq!(bluetooth.label, "Bluetooth");
        let ipod = options.iter().find(|o| o.code == "USB/IPOD").unwrap();
        assert_eq!(ipod.label, "USB / iPod");
    }

    #[test]
    fn disabled_sources_are_hidden() {
        let (inner, events) = shared();
        apply("SSSODCD USE", &events, &inner);
        apply("SSSODBD USE", &events, &inner);
        // The owner has since deleted CD on the receiver.
        apply("SSSODCD DEL", &events, &inner);

        let options = inner.lock().unwrap().sources.options();
        assert!(options.iter().all(|o| o.code != "CD"));
        assert!(options.iter().any(|o| o.code == "BD"));
    }

    #[test]
    fn re_querying_does_not_duplicate_inputs() {
        // Every reconnect re-runs the source queries.
        let (inner, events) = shared();
        for _ in 0..3 {
            for line in SOURCE_REPLIES {
                apply(line, &events, &inner);
            }
        }
        let options = inner.lock().unwrap().sources.options();
        let mut codes: Vec<&str> = options.iter().map(|o| o.code.as_str()).collect();
        let total = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), total, "input list must not accumulate duplicates");
    }

    #[test]
    fn the_input_list_is_empty_until_the_receiver_answers() {
        // Callers must be able to tell "not known yet" from "has no inputs".
        let (inner, _events) = shared();
        assert!(inner.lock().unwrap().sources.options().is_empty());
    }

    #[test]
    fn losing_the_connection_clears_stale_state() {
        // Reporting the last volume we saw as current, while disconnected,
        // would put a number on screen that nothing is backing.
        let (inner, events) = shared();
        apply("PWON", &events, &inner);
        apply("MV57", &events, &inner);
        set_reachable(&inner, &events, false);

        let state = inner.lock().unwrap().state.clone();
        assert!(!state.reachable);
        assert_eq!(state.power, None);
        assert_eq!(state.volume, None);
    }

    fn receiver_for_tests() -> DenonReceiver {
        let found = Discovered {
            id: DeviceId::new("denon:test"),
            vendor: Vendor::Denon,
            name: "Test".into(),
            model: None,
            // TEST-NET-1: guaranteed not to be a real receiver.
            addr: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
            mac: None,
        };
        DenonReceiver::connect(&found)
    }

    #[tokio::test]
    async fn toggle_resolves_against_known_state() {
        let receiver = receiver_for_tests();
        apply("PWON", &receiver.events, &receiver.inner);
        assert_eq!(
            receiver.resolve(Command::Power(PowerState::Toggle)).unwrap(),
            Command::Power(PowerState::Off)
        );

        apply("MUOFF", &receiver.events, &receiver.inner);
        assert_eq!(
            receiver
                .resolve(Command::Volume(VolumeCmd::ToggleMute))
                .unwrap(),
            Command::Volume(VolumeCmd::Mute(true))
        );
    }

    #[tokio::test]
    async fn toggle_refuses_to_guess_when_state_is_unknown() {
        // Guessing here could silently power the room off mid-film.
        let receiver = receiver_for_tests();
        assert!(receiver.resolve(Command::Power(PowerState::Toggle)).is_err());
        assert!(
            receiver
                .resolve(Command::Volume(VolumeCmd::ToggleMute))
                .is_err()
        );
    }

    #[tokio::test]
    async fn commands_to_an_unreachable_receiver_fail_rather_than_hang() {
        let receiver = receiver_for_tests();
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            receiver.send(Command::Volume(VolumeCmd::Up)),
        )
        .await;
        assert!(result.is_ok(), "the call must not hang");
        assert!(matches!(result.unwrap(), Err(Error::Unreachable(_))));
    }
}
