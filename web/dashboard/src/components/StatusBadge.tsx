interface StatusBadgeProps {
  status: "running" | "idle" | "stopped";
}

export default function StatusBadge({ status }: StatusBadgeProps) {
  return (
    <span className={`status-badge ${status}`}>
      <span className="status-dot" />
      {status}
    </span>
  );
}
