import type { Command, Device, NavKey, TransportKey } from "./api";

interface Props {
  volumeDevice: Device | null;
  remoteDevice: Device | null;
  onSend: (deviceId: string, command: Command) => void;
}

export function Remote({ volumeDevice, remoteDevice, onSend }: Props) {
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
          <button
            aria-label="Volume down"
            disabled={!volumeDevice}
            onClick={() => sendVolume({ volume: "down" })}
          >
            −
          </button>
          <button
            aria-label="Volume up"
            disabled={!volumeDevice}
            onClick={() => sendVolume({ volume: "up" })}
          >
            +
          </button>
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
          <button aria-label="Up" disabled={!remoteDevice} onClick={() => sendNav("up")}>▲</button>
          <span className="spacer" />

          <button aria-label="Left" disabled={!remoteDevice} onClick={() => sendNav("left")}>◀</button>
          <button className="ok" disabled={!remoteDevice} onClick={() => sendNav("select")}>OK</button>
          <button aria-label="Right" disabled={!remoteDevice} onClick={() => sendNav("right")}>▶</button>

          <span className="spacer" />
          <button aria-label="Down" disabled={!remoteDevice} onClick={() => sendNav("down")}>▼</button>
          <span className="spacer" />
        </div>

        <div className="row" style={{ marginTop: 12 }}>
          <button disabled={!remoteDevice} onClick={() => sendNav("back")}>Back</button>
          <button disabled={!remoteDevice} onClick={() => sendNav("home")}>Home</button>
          <button disabled={!remoteDevice} onClick={() => sendNav("menu")}>Menu</button>
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
