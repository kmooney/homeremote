import { useEffect, useRef } from "react";
import type { Command, Device, NavKey, TransportKey } from "./api";
import { Inputs } from "./Inputs";
import { HoldButton, REPEAT_INTERVAL } from "./HoldButton";
import { TextEntry } from "./TextEntry";

/** Laptop keys that drive the Apple TV while the remote tab is open. */
const KEY_COMMANDS: Record<string, Command> = {
  ArrowUp: { nav: "up" },
  ArrowDown: { nav: "down" },
  ArrowLeft: { nav: "left" },
  ArrowRight: { nav: "right" },
  Enter: { nav: "select" },
  Escape: { nav: "back" },
  Backspace: { nav: "back" },
  " ": { transport: "play-pause" },
};

/** Whether a key is meant for a field on the page rather than the TV. */
function isTyping(target: EventTarget | null): boolean {
  return (
    target instanceof HTMLElement &&
    (target.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName))
  );
}

interface Props {
  volumeDevice: Device | null;
  remoteDevice: Device | null;
  inputDevice: Device | null;
  onSend: (deviceId: string, command: Command) => Promise<void>;
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

  // Kept in a ref so the listener, bound once, always sees the current device.
  const sendKey = useRef<(command: Command) => void>(() => {});
  sendKey.current = (command) => {
    if (remoteDevice) onSend(remoteDevice.id, command);
  };

  useEffect(() => {
    let lastRepeat = 0;
    const onKeyDown = (event: KeyboardEvent) => {
      // Leave browser shortcuts (Cmd+R, Ctrl+L, ...) and text fields alone.
      if (event.metaKey || event.ctrlKey || event.altKey || isTyping(event.target)) return;
      const command = KEY_COMMANDS[event.key];
      if (!command) return;
      // Otherwise Enter also clicks the focused button and arrows scroll the page.
      event.preventDefault();
      // Held keys auto-repeat faster than the Apple TV absorbs presses; pace
      // them like a held button so nothing plays out after the key is released.
      if (event.repeat) {
        if (event.timeStamp - lastRepeat < REPEAT_INTERVAL) return;
        lastRepeat = event.timeStamp;
      }
      sendKey.current(command);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

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

        {remoteDevice?.state.keyboard != null && (
          <TextEntry
            text={remoteDevice.state.keyboard}
            onChange={(text) => onSend(remoteDevice.id, { text })}
          />
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
