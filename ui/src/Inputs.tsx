import { useState } from "react";
import type { Command, Device } from "./api";

interface Props {
  device: Device | null;
  onSend: (deviceId: string, command: Command) => void;
}

/**
 * Source selection for the receiver, collapsed by default.
 *
 * Input changes are rare next to volume, so the list stays out of the way and
 * the summary carries the one fact worth seeing at a glance: what is selected.
 * Labels come from the device, so they read the same here as on its own
 * display — this receiver has MPLAY renamed "Cameras".
 */
export function Inputs({ device, onSend }: Props) {
  const [open, setOpen] = useState(false);
  if (!device) return null;

  const current = device.state.input;
  const currentLabel =
    device.inputs.find((i) => i.code === current)?.label ?? current ?? "—";

  const choose = (code: string) => {
    onSend(device.id, { input: code });
    // Collapse on choice: the job is done, and the summary now shows it.
    setOpen(false);
  };

  return (
    <div className="card">
      <button
        className="disclosure"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        <span className={open ? "triangle open" : "triangle"} aria-hidden="true">
          ▶
        </span>
        <span className="disclosure-label">Input</span>
        <span className="disclosure-value">{currentLabel}</span>
      </button>

      {open &&
        (device.inputs.length === 0 ? (
          <p className="empty">
            {device.state.reachable
              ? "Waiting for the receiver to list its inputs..."
              : "Not reachable."}
          </p>
        ) : (
          <div className="inputs">
            {device.inputs.map((input) => (
              <button
                key={input.code}
                className="input"
                aria-current={input.code === current}
                onClick={() => choose(input.code)}
              >
                {input.label}
              </button>
            ))}
          </div>
        ))}
    </div>
  );
}
