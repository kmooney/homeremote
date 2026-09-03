import { useState } from "react";
import { api, type Device } from "./api";

interface Props {
  devices: Device[];
  onError: (message: string) => void;
}

export function Devices({ devices, onError }: Props) {
  const [scanning, setScanning] = useState(false);
  /** Which device is mid-pairing, and the PIN being typed. */
  const [pairing, setPairing] = useState<string | null>(null);
  const [pin, setPin] = useState("");
  const [busy, setBusy] = useState(false);

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

  const startPairing = async (device: Device) => {
    setBusy(true);
    try {
      await api.pairStart(device.id);
      setPairing(device.id);
      setPin("");
    } catch (e) {
      onError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const finishPairing = async (device: Device) => {
    setBusy(true);
    try {
      await api.pairFinish(device.id, pin);
      setPairing(null);
      setPin("");
    } catch (e) {
      onError((e as Error).message);
      // The device tears the session down on a bad PIN, so make them start over
      // rather than retrying into a dead connection.
      setPairing(null);
    } finally {
      setBusy(false);
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

      {devices.map((device) => {
        const needsPairing = device.unavailableBecause?.includes("not paired");
        return (
          <div className="device" key={device.id}>
            <div style={{ flex: 1 }}>
              <div className="name">{device.name}</div>
              <div className="meta">
                {device.addr ?? "address unknown"} · {device.kind.replace("-", " ")}
              </div>
              {device.unavailableBecause && (
                <div className="warn">{device.unavailableBecause}</div>
              )}

              {needsPairing && pairing !== device.id && (
                <button
                  className="pair"
                  disabled={busy}
                  onClick={() => startPairing(device)}
                >
                  Pair
                </button>
              )}

              {pairing === device.id && (
                <div className="pairing">
                  <p className="meta">
                    Enter the PIN on the TV. It expires quickly, so be quick.
                  </p>
                  <div className="pinrow">
                    <input
                      className="pin"
                      inputMode="numeric"
                      pattern="[0-9]*"
                      autoFocus
                      maxLength={8}
                      placeholder="0000"
                      value={pin}
                      onChange={(e) => setPin(e.target.value.replace(/\D/g, ""))}
                      onKeyDown={(e) => {
                        if (e.key === "Enter" && pin.length >= 4) finishPairing(device);
                      }}
                    />
                    <button
                      disabled={busy || pin.length < 4}
                      onClick={() => finishPairing(device)}
                    >
                      {busy ? "..." : "Submit"}
                    </button>
                  </div>
                </div>
              )}
            </div>

            <div className="meta" style={{ textAlign: "right", whiteSpace: "nowrap" }}>
              {device.state.reachable ? "online" : "offline"}
              {device.state.input && <div>{device.state.input}</div>}
            </div>
          </div>
        );
      })}

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
