import { useState } from "react";
import { api, type Device } from "./api";

interface Props {
  devices: Device[];
  onError: (message: string) => void;
}

export function Devices({ devices, onError }: Props) {
  const [scanning, setScanning] = useState(false);

  const rescan = async () => {
    setScanning(true);
    try {
      await api.discover();
    } catch (e) {
      onError((e as Error).message);
    } finally {
      setScanning(false);
    }
  };

  return (
    <div className="card">
      <h2>Devices</h2>

      {devices.length === 0 && (
        <p className="empty">
          Nothing found yet. Check everything is on the same network, then scan.
        </p>
      )}

      {devices.map((device) => (
        <div className="device" key={device.id}>
          <div>
            <div className="name">{device.name}</div>
            <div className="meta">
              {device.addr ?? "address unknown"} · {device.kind.replace("-", " ")}
            </div>
            {device.unavailableBecause && (
              <div className="warn">{device.unavailableBecause}</div>
            )}
          </div>
          <div className="meta" style={{ textAlign: "right", whiteSpace: "nowrap" }}>
            {device.state.reachable ? "online" : "offline"}
            {device.state.input && <div>{device.state.input}</div>}
          </div>
        </div>
      ))}

      <button
        style={{ width: "100%", marginTop: 16 }}
        disabled={scanning}
        onClick={rescan}
      >
        {scanning ? "Scanning..." : "Scan for devices"}
      </button>
    </div>
  );
}
