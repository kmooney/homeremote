//! Running Harmony-style activities.
//!
//! Two behaviours matter more than the sequencing itself:
//!
//! - **Steps are skipped when the device is already in the wanted state.** So
//!   switching from one activity to another doesn't power-cycle the TV that
//!   both of them use.
//! - **Delays are load-bearing.** The receiver ignores input commands for a
//!   couple of seconds after power-on, which is the classic "worked when I
//!   tested it by hand" failure.

use std::sync::Arc;
use std::time::Duration;

use homeremote::config::{Activity, Step};
use homeremote::device::{Command, Device, PowerState, VolumeCmd};

use crate::state::{ActivityProgress, ActivityStatus, AppState, ServerEvent};

/// How many times a step is retried before the activity gives up.
const STEP_ATTEMPTS: usize = 3;

/// Gap between retries. Long enough for a device still waking up.
const RETRY_DELAY: Duration = Duration::from_millis(750);

#[derive(Debug, thiserror::Error)]
pub enum ActivityError {
    #[error("no activity called {0:?}")]
    Unknown(String),
    #[error("step {step} of {total} ({device}) failed: {source}")]
    Step {
        step: usize,
        total: usize,
        device: String,
        #[source]
        source: homeremote::Error,
    },
    #[error("step {step} of {total} refers to unknown device {device:?}")]
    UnknownDevice {
        step: usize,
        total: usize,
        device: String,
    },
}

/// Run an activity to completion.
///
/// Progress is published as it goes, so the UI can show which step is running
/// and which one failed rather than just spinning.
pub async fn run(state: &Arc<AppState>, id: &str) -> Result<(), ActivityError> {
    let activity = state
        .config
        .read()
        .await
        .activity(id)
        .cloned()
        .ok_or_else(|| ActivityError::Unknown(id.to_string()))?;

    let total = activity.steps.len();
    publish(state, &activity, ActivityStatus::Started, None, total, None);

    for (index, step) in activity.steps.iter().enumerate() {
        let number = index + 1;
        let Some(device) = state.device(&step.device).await else {
            let error = ActivityError::UnknownDevice {
                step: number,
                total,
                device: step.device.to_string(),
            };
            publish(
                state,
                &activity,
                ActivityStatus::Failed,
                Some(number),
                total,
                Some(error.to_string()),
            );
            // Leave the activity unset: the room is in an unknown state and
            // claiming otherwise would make the next switch skip real work.
            *state.current_activity.write().await = None;
            return Err(error);
        };

        if already_satisfied(device.as_ref(), &step.command).await {
            tracing::debug!(step = number, device = %step.device, "already in the wanted state");
            publish(
                state,
                &activity,
                ActivityStatus::Skipped,
                Some(number),
                total,
                None,
            );
            continue;
        }

        publish(
            state,
            &activity,
            ActivityStatus::Running,
            Some(number),
            total,
            Some(format!("{} on {}", step.command.describe(), device.name())),
        );

        if let Err(source) = attempt(device.as_ref(), step).await {
            let error = ActivityError::Step {
                step: number,
                total,
                device: device.name().to_string(),
                source,
            };
            publish(
                state,
                &activity,
                ActivityStatus::Failed,
                Some(number),
                total,
                Some(error.to_string()),
            );
            *state.current_activity.write().await = None;
            return Err(error);
        }

        // Give the device time to act before the next step assumes it has.
        if let Some(delay) = step.delay_ms {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
    }

    *state.current_activity.write().await = Some(activity.id.clone());
    publish(
        state,
        &activity,
        ActivityStatus::Finished,
        None,
        total,
        None,
    );
    Ok(())
}

/// Send one step, retrying a device that is probably still waking up.
async fn attempt(device: &dyn Device, step: &Step) -> homeremote::Result<()> {
    let mut last = None;
    for attempt in 1..=STEP_ATTEMPTS {
        match device.send(step.command.clone()).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::debug!(attempt, error = %e, device = device.name(), "step failed");
                last = Some(e);
                if attempt < STEP_ATTEMPTS {
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        }
    }
    Err(last.expect("at least one attempt was made"))
}

/// Whether a command would be a no-op given what we know about the device.
///
/// Only returns true on *positive* knowledge. An unknown state means the step
/// runs — sending a redundant command is harmless, skipping a necessary one is
/// not.
async fn already_satisfied(device: &dyn Device, command: &Command) -> bool {
    let state = device.state().await;
    if !state.reachable {
        return false;
    }
    match command {
        Command::Power(PowerState::On) => state.power == Some(true),
        Command::Power(PowerState::Off) => state.power == Some(false),
        Command::Input(wanted) => state
            .input
            .as_deref()
            .is_some_and(|current| current.eq_ignore_ascii_case(wanted)),
        Command::Volume(VolumeCmd::Mute(wanted)) => state.muted == Some(*wanted),
        // A relative volume change, a nav key, or a transport key is never
        // redundant — it means "do this now", not "be in this state".
        _ => false,
    }
}

fn publish(
    state: &Arc<AppState>,
    activity: &Activity,
    status: ActivityStatus,
    step: Option<usize>,
    total: usize,
    message: Option<String>,
) {
    state.publish(ServerEvent::Activity(ActivityProgress {
        activity: activity.id.clone(),
        status,
        step,
        total: Some(total),
        message,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use homeremote::device::{
        Capabilities, DeviceEvent, DeviceId, DeviceKind, DeviceState, NavKey,
    };
    use std::sync::Mutex;
    use tokio::sync::broadcast;

    /// A device that records what it was sent and reports a state we control.
    struct FakeDevice {
        id: DeviceId,
        state: Mutex<DeviceState>,
        sent: Mutex<Vec<Command>>,
        events: broadcast::Sender<DeviceEvent>,
    }

    impl FakeDevice {
        fn new(state: DeviceState) -> Self {
            let (events, _) = broadcast::channel(8);
            Self {
                id: DeviceId::new("fake"),
                state: Mutex::new(state),
                sent: Mutex::new(Vec::new()),
                events,
            }
        }
    }

    #[async_trait::async_trait]
    impl Device for FakeDevice {
        fn id(&self) -> &DeviceId {
            &self.id
        }
        fn kind(&self) -> DeviceKind {
            DeviceKind::Receiver
        }
        fn name(&self) -> &str {
            "Fake"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        async fn send(&self, cmd: Command) -> homeremote::Result<()> {
            self.sent.lock().unwrap().push(cmd);
            Ok(())
        }
        async fn state(&self) -> DeviceState {
            self.state.lock().unwrap().clone()
        }
        fn subscribe(&self) -> broadcast::Receiver<DeviceEvent> {
            self.events.subscribe()
        }
    }

    fn on_state() -> DeviceState {
        DeviceState {
            reachable: true,
            power: Some(true),
            volume: Some(0.5),
            muted: Some(false),
            input: Some("MPLAY".into()),
        }
    }

    #[tokio::test]
    async fn skips_power_on_for_a_device_already_on() {
        // This is what stops an activity switch from power-cycling the TV.
        let device = FakeDevice::new(on_state());
        assert!(already_satisfied(&device, &Command::Power(PowerState::On)).await);
        assert!(!already_satisfied(&device, &Command::Power(PowerState::Off)).await);
    }

    #[tokio::test]
    async fn skips_an_input_that_is_already_selected() {
        let device = FakeDevice::new(on_state());
        assert!(already_satisfied(&device, &Command::Input("MPLAY".into())).await);
        // Case shouldn't matter; the receiver reports upper case.
        assert!(already_satisfied(&device, &Command::Input("mplay".into())).await);
        assert!(!already_satisfied(&device, &Command::Input("BD".into())).await);
    }

    #[tokio::test]
    async fn never_skips_when_state_is_unknown() {
        // Skipping a needed step is far worse than repeating a harmless one.
        let device = FakeDevice::new(DeviceState {
            reachable: true,
            ..DeviceState::default()
        });
        assert!(!already_satisfied(&device, &Command::Power(PowerState::On)).await);
        assert!(!already_satisfied(&device, &Command::Input("MPLAY".into())).await);
    }

    #[tokio::test]
    async fn never_skips_for_an_unreachable_device() {
        let device = FakeDevice::new(DeviceState {
            reachable: false,
            power: Some(true),
            ..DeviceState::default()
        });
        assert!(!already_satisfied(&device, &Command::Power(PowerState::On)).await);
    }

    #[tokio::test]
    async fn relative_and_momentary_commands_are_never_skipped() {
        // "Volume up" means do it now, not be in a state.
        let device = FakeDevice::new(on_state());
        assert!(!already_satisfied(&device, &Command::Volume(VolumeCmd::Up)).await);
        assert!(!already_satisfied(&device, &Command::Volume(VolumeCmd::Set(0.5))).await);
        assert!(!already_satisfied(&device, &Command::Nav(NavKey::Home)).await);
    }

    #[tokio::test]
    async fn mute_is_skipped_only_when_it_matches() {
        let device = FakeDevice::new(on_state());
        assert!(already_satisfied(&device, &Command::Volume(VolumeCmd::Mute(false))).await);
        assert!(!already_satisfied(&device, &Command::Volume(VolumeCmd::Mute(true))).await);
    }

    #[tokio::test]
    async fn a_step_is_retried_before_being_given_up_on() {
        struct Flaky {
            id: DeviceId,
            failures_left: Mutex<usize>,
            events: broadcast::Sender<DeviceEvent>,
        }
        #[async_trait::async_trait]
        impl Device for Flaky {
            fn id(&self) -> &DeviceId {
                &self.id
            }
            fn kind(&self) -> DeviceKind {
                DeviceKind::Receiver
            }
            fn name(&self) -> &str {
                "Flaky"
            }
            fn capabilities(&self) -> Capabilities {
                Capabilities::default()
            }
            async fn send(&self, _: Command) -> homeremote::Result<()> {
                let mut left = self.failures_left.lock().unwrap();
                if *left > 0 {
                    *left -= 1;
                    return Err(homeremote::Error::Timeout);
                }
                Ok(())
            }
            async fn state(&self) -> DeviceState {
                DeviceState::default()
            }
            fn subscribe(&self) -> broadcast::Receiver<DeviceEvent> {
                self.events.subscribe()
            }
        }

        let (events, _) = broadcast::channel(8);
        // Fails twice, succeeds on the third attempt — a receiver still waking.
        let device = Flaky {
            id: DeviceId::new("flaky"),
            failures_left: Mutex::new(2),
            events,
        };
        let step = Step {
            device: DeviceId::new("flaky"),
            command: Command::Power(PowerState::On),
            delay_ms: None,
        };
        assert!(attempt(&device, &step).await.is_ok());
    }

    #[tokio::test]
    async fn a_step_that_never_succeeds_reports_the_last_error() {
        let device = FakeDevice::new(on_state());
        struct AlwaysFails(DeviceId, broadcast::Sender<DeviceEvent>);
        #[async_trait::async_trait]
        impl Device for AlwaysFails {
            fn id(&self) -> &DeviceId {
                &self.0
            }
            fn kind(&self) -> DeviceKind {
                DeviceKind::Receiver
            }
            fn name(&self) -> &str {
                "Broken"
            }
            fn capabilities(&self) -> Capabilities {
                Capabilities::default()
            }
            async fn send(&self, _: Command) -> homeremote::Result<()> {
                Err(homeremote::Error::Timeout)
            }
            async fn state(&self) -> DeviceState {
                DeviceState::default()
            }
            fn subscribe(&self) -> broadcast::Receiver<DeviceEvent> {
                self.1.subscribe()
            }
        }
        let (events, _) = broadcast::channel(8);
        let broken = AlwaysFails(DeviceId::new("broken"), events);
        let step = Step {
            device: DeviceId::new("broken"),
            command: Command::Power(PowerState::On),
            delay_ms: None,
        };
        assert!(attempt(&broken, &step).await.is_err());
        drop(device);
    }
}
