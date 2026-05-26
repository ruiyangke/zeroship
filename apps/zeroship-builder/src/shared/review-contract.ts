export const CRITIC_DIMENSIONS = [
  "composed-from-system",
  "states",
  "responsive",
  "accessibility",
  "content",
  "correctness",
  "security",
  "performance",
  "code_health",
] as const;

export type CriticDimensionKey = (typeof CRITIC_DIMENSIONS)[number];

export const CRITIC_DIMENSION_LABELS: Record<CriticDimensionKey, string> = {
  "composed-from-system": "Composed from system",
  states: "States",
  responsive: "Responsive",
  accessibility: "Accessibility",
  content: "Content",
  correctness: "Correctness",
  security: "Security",
  performance: "Performance",
  code_health: "Code health",
};

export const REVIEWER_BLOCKER_KINDS = [
  "security",
  "correctness",
  "destructive_op",
  "secrets_in_client",
  "auth_bypass",
  "injection",
  "xss",
  "dangerous_html_user_content",
  "missing_critical_states",
  "serious_accessibility",
  "migration_safety",
  "build_or_typecheck",
] as const;

export type ReviewerBlockerKind = (typeof REVIEWER_BLOCKER_KINDS)[number];

export const REVIEWER_HARD_GATE_SEVERITIES = [
  "high",
  "critical",
] as const;
