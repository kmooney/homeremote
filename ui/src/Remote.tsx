import type { Command, Device, NavKey, TransportKey } from "./api";
import { Inputs } from "./Inputs";
import { HoldButton } from "./HoldButton";

interface Props {
  volumeDevice: Device | null;
  remoteDevice: Device | null;
  inputDevice: Device | null;
  onSend: (deviceId: string, command: Command) => void;
}

export function Remote({ volumeDevice, remoteDevice, inputDevice, onSend }: Props) {
  const volume = volumeDevice?.state.volume;
  const muted = volumeDevice?.state.muted ?? false;

  const sendVolume = (command: Command) => {
    if (volumeDevice) onSend(volumeDevice.id, command);
  };
  const sendNav = (key: NavKey) => {
    if (remoteDevice) onSend(remoteDevice.id, { nav: key });
  };
  const sendTransport = (key: TransportKey) => {
    if (remoteDevice) onSend(remoteDevice.id, { transport: key });
  };

  return (
    <>
      <Inputs device={inputDevice} onSend={onSend} />

      <div className="card">
        <h2>{volumeDevice ? `Volume — ${volumeDevice.name}` : "Volume"}</h2>

        <div className="level">
          {volume == null ? "—" : `${Math.round(volume * 100)}%`}
          <small>
            {!volumeDevice
              ? "no device can control volume"
              : !volumeDevice.state.reachable
                ? "not reachable"
                : muted
                  ? "muted"
                  : (volumeDevice.state.input ?? "")}
          </small>
        </div>

        <div className="volume">
          <HoldButton
            aria-label="Volume down"
            disabled={!volumeDevice}
            onPress={() => sendVolume({ volume: "down" })}
          >
            −
          </HoldButton>
          <HoldButton
            aria-label="Volume up"
            disabled={!volumeDevice}
            onPress={() => sendVolume({ volume: "up" })}
          >
            +
          </HoldButton>
          <button
            className="mute"
            aria-pressed={muted}
            disabled={!volumeDevice}
            onClick={() => sendVolume({ volume: { mute: !muted } })}
          >
            {muted ? "Unmute" : "Mute"}
          </button>
        </div>
      </div>

      <div className="card">
        <h2>{remoteDevice ? `Remote — ${remoteDevice.name}` : "Remote"}</h2>

        {!remoteDevice && (
          <p className="empty">No device here handles navigation yet.</p>
        )}

        <div className="dpad">
          <span className="spacer" />
          <HoldButton aria-label="Up" disabled={!remoteDevice} onPress={() => sendNav("up")}>▲</HoldButton>
          <span className="spacer" />

          <HoldButton aria-label="Left" disabled={!remoteDevice} onPress={() => sendNav("left")}>◀</HoldButton>
          <button className="ok" disabled={!remoteDevice} onClick={() => sendNav("select")}>OK</button>
          <HoldButton aria-label="Right" disabled={!remoteDevice} onPress={() => sendNav("right")}>▶</HoldButton>

          <span className="spacer" />
          <HoldButton aria-label="Down" disabled={!remoteDevice} onPress={() => sendNav("down")}>▼</HoldButton>
          <span className="spacer" />
        </div>

        {/* Back and Menu are the same tvOS command, so only one is shown. */}
        <div className="row" style={{ marginTop: 12 }}>
          <button disabled={!remoteDevice} onClick={() => sendNav("back")}>Back</button>
          <button disabled={!remoteDevice} onClick={() => sendNav("home")}>Home</button>
        </div>

        <div className="row">
          <button aria-label="Previous" disabled={!remoteDevice} onClick={() => sendTransport("previous")}>⏮</button>
          <button aria-label="Play or pause" disabled={!remoteDevice} onClick={() => sendTransport("play-pause")}>⏯</button>
          <button aria-label="Next" disabled={!remoteDevice} onClick={() => sendTransport("next")}>⏭</button>
        </div>
      </div>
    </>
  );
}
