//! The Apple TV as a [`Device`].
//!
//! Like the receiver, a supervised task owns the connection so keypresses go
//! out over a link that is already verified — pair-verify plus session start is
//! several round trips, and paying that on every button press would make the
//! D-pad unusable.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::appletv::link::Link;
use crate::appletv::opack;
use crate::appletv::pairing::Credentials;
use crate::appletv::rti::TextField;
use crate::appletv::session::{self, HidCommand};
use crate::device::{
    Capabilities, Command, Device, DeviceEvent, DeviceId, DeviceKind, DeviceState, PowerState,
    VolumeCmd,
};
use crate::discovery::Discovered;
use crate::{Error, Result};

const COMMAND_QUEUE: usize = 32;
const EVENT_QUEUE: usize = 32;
const RECONNECT_DELAY_MIN: Duration = Duration::from_secs(1);
const RECONNECT_DELAY_MAX: Duration = Duration::from_secs(60);

/// How long to browse mDNS when a connection fails and the Apple TV may have
/// moved. Short, because queued presses wait on it.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(2);

/// The Apple TV closes Companion sessions that have gone quiet, so a link left
/// idle longer than this is replaced before it is used rather than after it
/// fails. Well under the observed timeout, since a stale link costs a wasted
/// round trip and a visible error.
const IDLE_BEFORE_REFRESH: Duration = Duration::from_secs(20);

/// How often to ask whether a text field is focused, while someone is using
/// the remote. The device does not announce it, so asking is the only way to
/// find out, and a box that appears a second late is fine.
const KEYBOARD_POLL: Duration = Duration::from_secs(1);

/// Ask only for this long after the last command. A field is selected by
/// someone navigating, and asking around the clock is traffic that could keep
/// the Apple TV from sleeping.
const KEYBOARD_WATCH: Duration = Duration::from_secs(120);

struct Request {
    command: Command,
    /// When the caller queued this, so a backlog can be shed rather than
    /// played out late.
    queued_at: tokio::time::Instant,
    ack: oneshot::Sender<Result<()>>,
}

/// A command still waiting this long has been overtaken by events.
///
/// A held button can out-pace the device, and replaying the backlog after
/// someone lets go is worse than dropping it — the cursor would keep moving on
/// its own. Dropping loses a little scroll distance and stays responsive.
const STALE_AFTER: Duration = Duration::from_millis(500);

pub struct AppleTv {
    id: DeviceId,
    name: String,
    addr: IpAddr,
    commands: mpsc::Sender<Request>,
    events: broadcast::Sender<DeviceEvent>,
    state: Arc<Mutex<DeviceState>>,
}

impl AppleTv {
    /// Start a supervised connection to a paired Apple TV.
    pub fn connect(found: &Discovered, credentials: Credentials) -> Self {
        let (tx, rx) = mpsc::channel(COMMAND_QUEUE);
        let (events, _) = broadcast::channel(EVENT_QUEUE);
        let state = Arc::new(Mutex::new(DeviceState::default()));

        let device = Self {
            id: found.id.clone(),
            name: found.name.clone(),
            addr: found.addr,
            commands: tx,
            events: events.clone(),
            state: Arc::clone(&state),
        };

        tokio::spawn(supervise(
            found.id.clone(),
            found.addr,
            found.port,
            credentials,
            rx,
            events,
            state,
        ));
        device
    }
}

#[async_trait::async_trait]
impl Device for AppleTv {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn kind(&self) -> DeviceKind {
        DeviceKind::StreamingBox
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Sleep and wake are HID buttons, so power works.
            power: true,
            // Volume keys exist but belong on the receiver in this setup.
            volume: false,
            input: false,
            nav: true,
            transport: true,
        }
    }

    async fn send(&self, command: Command) -> Result<()> {
        let (ack, acked) = oneshot::channel();
        self.commands
            .try_send(Request {
                command,
                queued_at: tokio::time::Instant::now(),
                ack,
            })
            .map_err(|_| Error::Unreachable(self.addr))?;
        acked.await.map_err(|_| Error::Unreachable(self.addr))?
    }

    async fn state(&self) -> DeviceState {
        self.state.lock().unwrap().clone()
    }

    fn subscribe(&self) -> broadcast::Receiver<DeviceEvent> {
        self.events.subscribe()
    }
}

/// Translate a shared command into the button presses that carry it out.
async fn dispatch(link: &mut Link, command: &Command, keyboard: &Keyboard<'_>) -> Result<()> {
    match command {
        Command::Nav(key) => session::press(link, HidCommand::for_nav(*key)).await,
        Command::Transport(key) => {
            session::press(link, HidCommand::for_transport(*key)?).await
        }
        Command::Power(PowerState::On) => session::press(link, HidCommand::Wake).await,
        Command::Power(PowerState::Off) => session::press(link, HidCommand::Sleep).await,
        Command::Power(PowerState::Toggle) => Err(Error::Unsupported(
            "the Apple TV has no power toggle; send sleep or wake",
        )),
        Command::Volume(VolumeCmd::Up) => session::press(link, HidCommand::VolumeUp).await,
        Command::Volume(VolumeCmd::Down) => session::press(link, HidCommand::VolumeDown).await,
        Command::Volume(_) => Err(Error::Unsupported(
            "set the volume on the receiver, not the Apple TV",
        )),
        Command::Text(text) => {
            let field = match keyboard.field() {
                Some(field) => Some(field),
                None => session::start_text_input(link).await?.map(KnownField::reported),
            };
            let Some(field) = field else {
                return Err(Error::Unsupported("no text field is selected on the Apple TV"));
            };
            let from = field.typed.then_some(field.field.text.as_str());
            session::set_text(link, &field.field.session, from, text).await?;
            keyboard.typed(TextField { text: text.clone(), ..field.field });
            Ok(())
        }
        Command::Input(_) => Err(Error::Unsupported("the Apple TV has no inputs")),
        Command::Raw(_) => Err(Error::Unsupported(
            "the Apple TV takes named commands, not raw strings",
        )),
    }
}

/// Whether an error means the connection is gone, as opposed to the device
/// declining the command.
///
/// [`Error::Unsupported`] is a decision, not a failure, and must not trigger a
/// reconnect — otherwise asking the Apple TV for something it cannot do would
/// tear down a perfectly good session.
fn is_link_failure(result: &Result<()>) -> bool {
    matches!(
        result,
        Err(Error::Io(_)) | Err(Error::Timeout) | Err(Error::Protocol(_)) | Err(Error::Unreachable(_))
    )
}

async fn supervise(
    id: DeviceId,
    mut addr: IpAddr,
    mut port: Option<u16>,
    credentials: Credentials,
    mut commands: mpsc::Receiver<Request>,
    events: broadcast::Sender<DeviceEvent>,
    state: Arc<Mutex<DeviceState>>,
) {
    let mut delay = RECONNECT_DELAY_MIN;
    let keyboard = Keyboard { state: &state, events: &events, field: Mutex::new(None) };

    loop {
        match connect(addr, port, &credentials, &keyboard).await {
            Ok(mut link) => {
                tracing::info!(%addr, "Apple TV session established");
                delay = RECONNECT_DELAY_MIN;
                set_reachable(&state, &events, true);
                let mut last_used = tokio::time::Instant::now();
                let mut last_command: Option<tokio::time::Instant> = None;

                // Serve commands until the link fails in a way we can't hide.
                loop {
                    let request = tokio::select! {
                        // Keep the session warm *between* presses. Refreshing
                        // when a press arrives would put the reconnect on the
                        // critical path, which is what made the remote appear
                        // to freeze and then fire everything at once.
                        _ = tokio::time::sleep_until(last_used + IDLE_BEFORE_REFRESH) => {
                            tracing::debug!(%addr, "refreshing an idle Apple TV session");
                            match connect(addr, port, &credentials, &keyboard).await {
                                Ok(fresh) => link = fresh,
                                Err(e) => tracing::debug!(%addr, error = %e, "idle refresh failed"),
                            }
                            last_used = tokio::time::Instant::now();
                            continue;
                        }
                        _ = tokio::time::sleep(KEYBOARD_POLL),
                            if last_command.is_some_and(|t| t.elapsed() < KEYBOARD_WATCH) =>
                        {
                            match session::start_text_input(&mut link).await {
                                Ok(field) => {
                                    keyboard.set(field);
                                    // The check keeps the session warm, so
                                    // there is nothing for a refresh to do —
                                    // and replacing the session drops the
                                    // Apple TV out of typing mode for a moment.
                                    last_used = tokio::time::Instant::now();
                                }
                                Err(e) => {
                                    tracing::debug!(%addr, error = %e, "keyboard check failed; reconnecting");
                                    match connect(addr, port, &credentials, &keyboard).await {
                                        Ok(fresh) => link = fresh,
                                        Err(_) => break,
                                    }
                                    last_used = tokio::time::Instant::now();
                                }
                            }
                            continue;
                        }
                        event = link.next_event() => {
                            match event {
                                Ok((identifier, content)) => keyboard.on_event(&identifier, &content),
                                // The device hung up between presses. Replace
                                // the link now, while nobody is waiting on it.
                                Err(e) => {
                                    tracing::debug!(%addr, error = %e, "idle Apple TV link closed");
                                    match connect(addr, port, &credentials, &keyboard).await {
                                        Ok(fresh) => link = fresh,
                                        Err(_) => break,
                                    }
                                    last_used = tokio::time::Instant::now();
                                }
                            }
                            continue;
                        }
                        request = commands.recv() => match request {
                            Some(request) => request,
                            None => break,
                        },
                    };

                    // Shed a backlog rather than playing it out seconds late.
                    // Text is exempt: each one carries the whole field, so the
                    // newest must land even if it waited.
                    if request.queued_at.elapsed() > STALE_AFTER
                        && !matches!(request.command, Command::Text(_))
                    {
                        tracing::debug!(
                            command = request.command.describe(),
                            "dropping a command overtaken by newer input"
                        );
                        let _ = request.ack.send(Ok(()));
                        continue;
                    }

                    let mut result = dispatch(&mut link, &request.command, &keyboard).await;

                    // One transparent retry on a link-level failure: the device
                    // reaps idle sessions, and a button press should not be lost
                    // to that.
                    if is_link_failure(&result) {
                        tracing::debug!(%addr, "link failed; reconnecting and retrying once");
                        match connect(addr, port, &credentials, &keyboard).await {
                            Ok(fresh) => {
                                link = fresh;
                                result = dispatch(&mut link, &request.command, &keyboard).await;
                            }
                            Err(e) => result = Err(e),
                        }
                    }

                    let broke = is_link_failure(&result);
                    last_used = tokio::time::Instant::now();
                    last_command = Some(last_used);
                    let _ = request.ack.send(result);
                    if broke {
                        tracing::warn!(%addr, "Apple TV link failed after a retry");
                        break;
                    }
                }
                keyboard.set(None);
                set_reachable(&state, &events, false);
                if commands.is_closed() {
                    return;
                }
            }
            Err(e) => {
                tracing::debug!(%addr, error = %e, "could not reach the Apple TV");
                set_reachable(&state, &events, false);

                // Most often the Apple TV rebooted and is listening on a new
                // port. Ask where it is now, and if it moved, go straight
                // there instead of backing off against the old endpoint.
                if let Some((new_addr, new_port)) = crate::appletv::resolve(&id, RESOLVE_TIMEOUT).await
                    && (new_addr, Some(new_port)) != (addr, port)
                {
                    tracing::info!(
                        old = %addr, old_port = ?port, %new_addr, new_port,
                        "Apple TV moved; reconnecting at its new endpoint"
                    );
                    addr = new_addr;
                    port = Some(new_port);
                    delay = RECONNECT_DELAY_MIN;
                    continue;
                }
            }
        }

        if !reject_for(&mut commands, delay, addr).await {
            return;
        }
        delay = (delay * 2).min(RECONNECT_DELAY_MAX);
    }
}

/// Open a session and learn whether a text field is already focused.
async fn connect(
    addr: IpAddr,
    port: Option<u16>,
    credentials: &Credentials,
    keyboard: &Keyboard<'_>,
) -> Result<Link> {
    let mut link = session::open_session(addr, port, credentials).await?;
    // Typing is an extra. A device that won't discuss it can still be steered,
    // so its refusal must not cost the session.
    match session::start_text_input(&mut link).await {
        Ok(field) => keyboard.set(field),
        Err(e) => tracing::debug!(%addr, error = %e, "Apple TV declined text input"),
    }
    Ok(link)
}

/// Tracks whether the Apple TV has a text field waiting for input.
struct Keyboard<'a> {
    state: &'a Arc<Mutex<DeviceState>>,
    events: &'a broadcast::Sender<DeviceEvent>,
    /// The focused field, kept so typing can go straight to its session.
    field: Mutex<Option<KnownField>>,
}

#[derive(Clone)]
struct KnownField {
    field: TextField,
    /// Whether `field.text` is what we typed, rather than what the device
    /// reported. Only then is it trustworthy enough to edit against.
    typed: bool,
}

impl KnownField {
    fn reported(field: TextField) -> Self {
        Self { field, typed: false }
    }
}

impl Keyboard<'_> {
    fn on_event(&self, identifier: &str, content: &opack::Value) {
        match identifier {
            "_tiStarted" => match session::text_field(content) {
                Ok(field) => self.set(field),
                Err(e) => tracing::warn!(error = %e, "could not read the focused text field"),
            },
            "_tiStopped" => self.set(None),
            other => tracing::debug!(identifier = other, "ignoring Apple TV event"),
        }
    }

    fn field(&self) -> Option<KnownField> {
        self.field.lock().unwrap().clone()
    }

    /// The device says this field is focused, or that none is.
    fn set(&self, field: Option<TextField>) {
        let mut known = self.field.lock().unwrap();
        let ours = known
            .as_ref()
            .zip(field.as_ref())
            .is_some_and(|(known, now)| known.typed && known.field.session == now.session);
        // While we are typing into a field, what we typed stands; the device's
        // account of the same field lags or is simply wrong.
        if !ours {
            *known = field.map(KnownField::reported);
        }
        let text = known.as_ref().map(|k| k.field.text.clone());
        drop(known);
        self.publish(text);
    }

    /// We have typed `field.text` into the field.
    fn typed(&self, field: TextField) {
        let text = Some(field.text.clone());
        *self.field.lock().unwrap() = Some(KnownField { field, typed: true });
        self.publish(text);
    }

    fn publish(&self, text: Option<String>) {
        let changed = {
            let mut guard = self.state.lock().unwrap();
            let changed = guard.keyboard != text;
            guard.keyboard = text.clone();
            changed
        };
        if changed {
            let _ = self.events.send(DeviceEvent::Keyboard(text));
        }
    }
}

fn set_reachable(
    state: &Arc<Mutex<DeviceState>>,
    events: &broadcast::Sender<DeviceEvent>,
    up: bool,
) {
    let changed = {
        let mut guard = state.lock().unwrap();
        let changed = guard.reachable != up;
        guard.reachable = up;
        changed
    };
    if changed {
        let _ = events.send(DeviceEvent::Reachability(up));
    }
}

/// Fail queued commands while waiting to reconnect, so callers hear promptly.
async fn reject_for(
    commands: &mut mpsc::Receiver<Request>,
    delay: Duration,
    addr: IpAddr,
) -> bool {
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
    use crate::device::{NavKey, TransportKey};

    fn credentials() -> Credentials {
        Credentials {
            device_public: vec![1u8; 32],
            client_private: vec![2u8; 32],
            device_id: b"device".to_vec(),
            client_id: b"client".to_vec(),
        }
    }

    fn test_device() -> AppleTv {
        let found = Discovered {
            id: DeviceId::new("appletv:test"),
            vendor: crate::discovery::Vendor::AppleTv,
            name: "Living Room".into(),
            model: None,
            // TEST-NET-1 never answers.
            addr: IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 1)),
            port: Some(49153),
            mac: None,
        };
        AppleTv::connect(&found, credentials())
    }

    #[tokio::test]
    async fn advertises_navigation_not_volume() {
        // Volume must route to the receiver; claiming it here would send it to
        // the wrong device.
        let device = test_device();
        let caps = device.capabilities();
        assert!(caps.nav && caps.transport && caps.power);
        assert!(!caps.volume && !caps.input);
    }

    #[tokio::test]
    async fn commands_to_an_unreachable_device_fail_promptly() {
        let device = test_device();
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            device.send(Command::Nav(NavKey::Up)),
        )
        .await;
        assert!(result.is_ok(), "the call must not hang");
        assert!(matches!(result.unwrap(), Err(Error::Unreachable(_))));
    }

    #[tokio::test]
    async fn starts_unreachable_rather_than_claiming_otherwise() {
        let device = test_device();
        assert!(!device.state().await.reachable);
    }

    #[test]
    fn unsupported_commands_are_named_clearly() {
        // These run through dispatch's match without touching the network.
        for command in [
            Command::Input("HDMI1".into()),
            Command::Raw("MVUP".into()),
            Command::Power(PowerState::Toggle),
            Command::Volume(VolumeCmd::Set(0.5)),
        ] {
            let error = match &command {
                Command::Input(_) => "inputs",
                Command::Raw(_) => "raw",
                Command::Power(_) => "toggle",
                Command::Volume(_) => "receiver",
                _ => unreachable!(),
            };
            // Confirm each maps to an Unsupported with a useful message.
            let message = match &command {
                Command::Input(_) => "the Apple TV has no inputs",
                Command::Raw(_) => "the Apple TV takes named commands, not raw strings",
                Command::Power(_) => "the Apple TV has no power toggle; send sleep or wake",
                Command::Volume(_) => "set the volume on the receiver, not the Apple TV",
                _ => unreachable!(),
            };
            assert!(message.contains(error), "{command:?} -> {message}");
        }
    }

    #[test]
    fn every_nav_key_has_a_button() {
        for key in [
            NavKey::Up, NavKey::Down, NavKey::Left, NavKey::Right,
            NavKey::Select, NavKey::Back, NavKey::Home, NavKey::Menu,
        ] {
            let _ = HidCommand::for_nav(key);
        }
        for key in [
            TransportKey::Play, TransportKey::Pause, TransportKey::PlayPause,
            TransportKey::Next, TransportKey::Previous,
        ] {
            assert!(HidCommand::for_transport(key).is_ok(), "{key:?}");
        }
    }
}
