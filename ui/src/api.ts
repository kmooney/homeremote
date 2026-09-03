/** Types and helpers for talking to the daemon. */

export type PowerState = "on" | "off" | "toggle";
export type NavKey =
  | "up" | "down" | "left" | "right"
  | "select" | "back" | "home" | "menu";
export type TransportKey =
  | "play" | "pause" | "play-pause" | "stop" | "next" | "previous";

/** Mirrors the Rust `Command` enum's serde representation. */
export type Command =
  | { power: PowerState }
  | { volume: "up" | "down" | { set: number } | { mute: boolean } | "toggle-mute" }
  | { input: string }
  | { nav: NavKey }
  | { transport: TransportKey }
  | { raw: string };

export interface Capabilities {
  power: boolean;
  volume: boolean;
  input: boolean;
  nav: boolean;
  transport: boolean;
}

export interface DeviceState {
  reachable: boolean;
  power: boolean | null;
  volume: number | null;
  muted: boolean | null;
  input: string | null;
}

export interface Device {
  id: string;
  name: string;
  vendor: "denon" | "samsung" | "apple-tv";
  kind: "receiver" | "television" | "streaming-box";
  addr: string | null;
  capabilities: Capabilities | null;
  unavailableBecause: string | null;
  state: DeviceState;
}

export interface Step {
  device: string;
  command: Command;
  delay_ms?: number;
}

export interface Activity {
  id: string;
  name: string;
  steps: Step[];
  volume_device?: string;
  remote_device?: string;
}

export type ActivityStatus =
  | "started" | "skipped" | "running" | "finished" | "failed";

export type DeviceEventPayload =
  | { power: boolean }
  | { volume: number }
  | { muted: boolean }
  | { input: string }
  | { reachability: boolean };

/** Messages pushed over the WebSocket. */
export type ServerEvent =
  | { type: "snapshot"; devices: Device[]; currentActivity: string | null }
  | { type: "device"; device: string; event: DeviceEventPayload }
  | {
      type: "activity";
      activity: string;
      status: ActivityStatus;
      step?: number;
      total?: number;
      message?: string;
    }
  | { type: "devices-changed" };

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, {
    headers: { "Content-Type": "application/json" },
    ...init,
  });
  if (!response.ok) {
    // The daemon reports failures as {"error": "..."}; surface that text so the
    // UI can say "receiver refused the connection" rather than "502".
    let detail = `${response.status} ${response.statusText}`;
    try {
      const body = await response.json();
      if (body?.error) detail = body.error;
    } catch {
      /* not JSON; keep the status line */
    }
    throw new Error(detail);
  }
  return response.status === 204 ? (undefined as T) : response.json();
}

export const api = {
  devices: () => request<{ devices: Device[] }>("/api/devices"),

  activities: () =>
    request<{ activities: Activity[]; current: string | null }>("/api/activities"),

  send: (deviceId: string, command: Command) =>
    request<void>(`/api/devices/${encodeURIComponent(deviceId)}/command`, {
      method: "POST",
      body: JSON.stringify({ command }),
    }),

  startActivity: (id: string) =>
    request<void>(`/api/activities/${encodeURIComponent(id)}/start`, {
      method: "POST",
    }),

  discover: () => request<{ devices: Device[] }>("/api/discover", { method: "POST" }),
};

/**
 * Subscribe to daemon events, reconnecting automatically.
 *
 * A remote that silently stops updating is worse than one that says it is
 * offline, so connection state is reported too.
 */
export function subscribe(
  onEvent: (event: ServerEvent) => void,
  onConnected: (connected: boolean) => void,
): () => void {
  let socket: WebSocket | null = null;
  let retry: ReturnType<typeof setTimeout> | null = null;
  let closed = false;
  let delay = 500;

  const connect = () => {
    if (closed) return;
    const protocol = location.protocol === "https:" ? "wss:" : "ws:";
    socket = new WebSocket(`${protocol}//${location.host}/api/events`);

    socket.onopen = () => {
      delay = 500;
      onConnected(true);
    };
    socket.onmessage = (message) => {
      try {
        onEvent(JSON.parse(message.data));
      } catch {
        /* ignore a frame we can't parse */
      }
    };
    socket.onclose = () => {
      onConnected(false);
      if (closed) return;
      // Back off so a daemon that is down doesn't get hammered.
      retry = setTimeout(connect, delay);
      delay = Math.min(delay * 2, 10_000);
    };
    socket.onerror = () => socket?.close();
  };

  connect();
  return () => {
    closed = true;
    if (retry) clearTimeout(retry);
    socket?.close();
  };
}
