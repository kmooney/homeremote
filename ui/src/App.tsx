import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  api,
  subscribe,
  type Activity,
  type Command,
  type Device,
  type ServerEvent,
} from "./api";
import { Activities } from "./Activities";
import { Remote } from "./Remote";
import { Devices } from "./Devices";

type Tab = "activities" | "remote" | "devices";

export function App() {
  const [devices, setDevices] = useState<Device[]>([]);
  const [activities, setActivities] = useState<Activity[]>([]);
  const [current, setCurrent] = useState<string | null>(null);
  const [connected, setConnected] = useState(false);
  const [tab, setTab] = useState<Tab>("activities");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  // Errors clear themselves; a stale message is worse than none.
  const errorTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const report = useCallback((message: string) => {
    setError(message);
    if (errorTimer.current) clearTimeout(errorTimer.current);
    errorTimer.current = setTimeout(() => setError(null), 5000);
  }, []);

  /** Apply one pushed event to local state, so the UI never polls. */
  const apply = useCallback((event: ServerEvent) => {
    switch (event.type) {
      case "snapshot":
        setDevices(event.devices);
        setCurrent(event.currentActivity);
        break;

      case "device":
        setDevices((previous) =>
          previous.map((device) => {
            if (device.id !== event.device) return device;
            const payload = event.event;
            const state = { ...device.state };
            if ("power" in payload) state.power = payload.power;
            if ("volume" in payload) state.volume = payload.volume;
            if ("muted" in payload) state.muted = payload.muted;
            if ("input" in payload) state.input = payload.input;
            if ("reachability" in payload) state.reachable = payload.reachability;
            return { ...device, state };
          }),
        );
        break;

      case "activity":
        if (event.status === "finished") {
          setCurrent(event.activity);
          setBusy(null);
        } else if (event.status === "failed") {
          setBusy(null);
          setCurrent(null);
          report(event.message ?? "The activity failed.");
        } else {
          setBusy(event.activity);
        }
        break;

      case "devices-changed":
        api.devices().then((r) => setDevices(r.devices)).catch(() => {});
        break;
    }
  }, [report]);

  useEffect(() => subscribe(apply, setConnected), [apply]);

  useEffect(() => {
    api.activities()
      .then((r) => {
        setActivities(r.activities);
        setCurrent(r.current);
      })
      .catch((e: Error) => report(e.message));
  }, [report]);

  const send = useCallback(
    async (deviceId: string, command: Command) => {
      try {
        await api.send(deviceId, command);
      } catch (e) {
        report((e as Error).message);
      }
    },
    [report],
  );

  const startActivity = useCallback(
    async (id: string) => {
      setBusy(id);
      try {
        await api.startActivity(id);
      } catch (e) {
        report((e as Error).message);
        setBusy(null);
      }
    },
    [report],
  );

  const activeActivity = useMemo(
    () => activities.find((a) => a.id === current) ?? null,
    [activities, current],
  );

  /**
   * Where volume goes. The running activity decides; otherwise fall back to
   * whichever device can actually do it — the receiver.
   */
  const volumeDevice = useMemo(() => {
    const named = activeActivity?.volume_device;
    return (
      devices.find((d) => d.id === named) ??
      devices.find((d) => d.capabilities?.volume) ??
      null
    );
  }, [devices, activeActivity]);

  /** Which device's inputs the remote offers — the receiver. */
  const inputDevice = useMemo(
    () => devices.find((d) => d.capabilities?.input) ?? null,
    [devices],
  );

  /** Where navigation and transport keys go. */
  const remoteDevice = useMemo(() => {
    const named = activeActivity?.remote_device;
    return (
      devices.find((d) => d.id === named) ??
      devices.find((d) => d.capabilities?.nav) ??
      null
    );
  }, [devices, activeActivity]);

  return (
    <div className="app">
      <header>
        <h1>Home Remote</h1>
        <span className="status">
          <span className={connected ? "dot up" : "dot"} />
          {connected ? "connected" : "offline"}
        </span>
      </header>

      <main>
        {tab === "activities" && (
          <Activities
            activities={activities}
            current={current}
            busy={busy}
            onStart={startActivity}
          />
        )}
        {tab === "remote" && (
          <Remote
            volumeDevice={volumeDevice}
            remoteDevice={remoteDevice}
            inputDevice={inputDevice}
            onSend={send}
          />
        )}
        {tab === "devices" && <Devices devices={devices} onError={report} />}
      </main>

      {error && <div className="toast" role="alert">{error}</div>}

      <nav>
        {(["activities", "remote", "devices"] as const).map((name) => (
          <button
            key={name}
            aria-current={tab === name}
            onClick={() => setTab(name)}
          >
            {name[0].toUpperCase() + name.slice(1)}
          </button>
        ))}
      </nav>
    </div>
  );
}
