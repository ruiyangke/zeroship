// ─── Skill catalogue (static) ───────────────────────────────────
//
// Per spec §5.3. The Skills surface is V1: a catalogue, not a real
// add-to-project flow. The backend (skill registry, install action,
// per-project skill manifest) is tracked as ISS-13 in /ISSUES.md.
// Until then we ship a static list so the marketing surface lands.

export type SkillCategory =
  | "communication"
  | "commerce"
  | "data"
  | "intelligence";

export interface Skill {
  slug: string;
  num: string;
  name: string;
  tagline: string;
  category: SkillCategory;
  /** Single emoji or short glyph used as the card "icon". */
  icon: string;
}

export const SKILLS: Skill[] = [
  {
    slug: "auth",
    num: "01",
    name: "Auth",
    tagline: "Sign-in with email + Google + GitHub. Sessions, OAuth, the lot.",
    category: "communication",
    icon: "🔑",
  },
  {
    slug: "email",
    num: "02",
    name: "Email",
    tagline: "Transactional email — confirmations, magic links, digests.",
    category: "communication",
    icon: "✉️",
  },
  {
    slug: "realtime",
    num: "03",
    name: "Realtime",
    tagline: "WebSockets and presence channels for live UI.",
    category: "communication",
    icon: "📡",
  },
  {
    slug: "payments",
    num: "04",
    name: "Payments",
    tagline: "Stripe Connect — one-time, subscriptions, marketplace splits.",
    category: "commerce",
    icon: "💳",
  },
  {
    slug: "photos",
    num: "05",
    name: "Photos",
    tagline: "Uploads, thumbnails, EXIF stripping, signed URLs.",
    category: "data",
    icon: "📷",
  },
  {
    slug: "search",
    num: "06",
    name: "Search",
    tagline: "Full-text search across your data with ranking and facets.",
    category: "data",
    icon: "🔎",
  },
  {
    slug: "ai",
    num: "07",
    name: "AI",
    tagline: "OpenAI / Claude / local — text, vision, embeddings, function-calls.",
    category: "intelligence",
    icon: "🧠",
  },
  {
    slug: "analytics",
    num: "08",
    name: "Analytics",
    tagline: "Event tracking, funnels, retention. Privacy-first by default.",
    category: "intelligence",
    icon: "📊",
  },
];

export const SKILL_CATEGORIES: { key: SkillCategory | "all"; label: string }[] = [
  { key: "all", label: "all" },
  { key: "communication", label: "communication" },
  { key: "commerce", label: "commerce" },
  { key: "data", label: "data" },
  { key: "intelligence", label: "intelligence" },
];
