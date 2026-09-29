import { useConnection } from "~/sync";

/** The full-window state before the first `ready`; the workspace is not rendered yet. */
export function Connecting() {
  const connection = useConnection();
  return (
    <div className="flex h-full flex-col items-center justify-center gap-1 bg-editor text-center">
      <div className="text-base text-muted">
        {connection.status === "syncing" ? "Syncing…" : "Connecting to arena0d…"}
      </div>
      {connection.attempt > 0 && (
        <div className="font-mono text-xs text-subtle">attempt {connection.attempt}</div>
      )}
      {connection.error !== null && <div className="text-xs text-subtle">{connection.error}</div>}
    </div>
  );
}
