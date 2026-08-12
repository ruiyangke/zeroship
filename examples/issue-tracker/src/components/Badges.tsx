// Small colored chips for the Bugzilla enums. Status and resolution are
// always rendered as two separate badges -- never merged into one string --
// per this app's fidelity requirement.
export function StatusBadge({ status }: { status: string }) {
  return <span className={`badge status-${status.toLowerCase()}`}>{status}</span>;
}

export function ResolutionBadge({ resolution }: { resolution: string | null }) {
  if (!resolution) return <span className="badge resolution-none">--</span>;
  return (
    <span className={`badge resolution-${resolution.toLowerCase()}`}>{resolution}</span>
  );
}

export function SeverityBadge({ severity }: { severity: string }) {
  return <span className={`badge severity-${severity.toLowerCase()}`}>{severity}</span>;
}

export function PriorityBadge({ priority }: { priority: string }) {
  return <span className={`badge priority-${priority.toLowerCase()}`}>{priority}</span>;
}

export function FlagChip({ typeName, status }: { typeName: string; status: string }) {
  return (
    <span className="chip flagchip" title={typeName}>
      {typeName}
      <b>{status}</b>
    </span>
  );
}
