import type { Command, Device } from "./api";

interface Props {
  device: Device | null;
  onSend: (deviceId: string, command: Command) => void;
}

/**
 * Source selection for the receiver.
 *
 * Labels come from the device, so they read the same here as they do on its
 * own display — this receiver has MPLAY renamed "Cameras".
 */
export function Inputs({ device, onSend }: Props) {
  if (!device) return null;

  const current = device.state.input;

  return (
    <div className="card">
      <h2>Input — {device.name}</h2>

      {device.inputs.length === 0 ? (
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
              onClick={() => onSend(device.id, { input: input.code })}
            >
              {input.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
