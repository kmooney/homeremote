import type { Activity } from "./api";

interface Props {
  activities: Activity[];
  current: string | null;
  busy: string | null;
  onStart: (id: string) => void;
}

export function Activities({ activities, current, busy, onStart }: Props) {
  if (activities.length === 0) {
    return (
      <div className="card">
        <h2>Activities</h2>
        <p className="empty">
          No activities configured yet. Add them to your{" "}
          <code>config.toml</code> — run <code>homeremote config</code> to find it.
        </p>
      </div>
    );
  }

  return (
    <div className="card">
      <h2>Activities</h2>
      <div className="activities">
        {activities.map((activity) => (
          <button
            key={activity.id}
            className="activity"
            aria-current={activity.id === current}
            disabled={busy !== null}
            onClick={() => onStart(activity.id)}
          >
            {busy === activity.id ? `Starting ${activity.name}...` : activity.name}
          </button>
        ))}
      </div>
    </div>
  );
}
