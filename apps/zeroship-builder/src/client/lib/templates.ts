// ─── Template catalogue ─────────────────────────────────────────
//
// 12 starting points for the new-project wizard, grouped by intent.
// Each template carries a default prompt the agent will receive as
// the first chat turn — the wizard step 2 lets the creator edit it.
//
// `slug` is used in the URL: /new?template=<slug>.

export type TemplateCategory = "sharing" | "collecting" | "selling" | "showing" | "internal";

export interface Template {
  slug: string;
  num: string;            // "01", "02", … — for the № marginalia label
  category: TemplateCategory;
  name: string;
  tagline: string;
  bullets: [string, string, string];
  estSeconds: number;
  /** Default first-turn prompt. The wizard pre-fills the textarea. */
  defaultPrompt: string;
}

export const TEMPLATES: Template[] = [
  {
    slug: "recipe-journal",
    num: "01",
    category: "sharing",
    name: "Recipe Journal",
    tagline: "A small space for recipes & photos.",
    bullets: ["posts with photos", "simple sign-in", "reactions & votes"],
    estSeconds: 30,
    defaultPrompt:
      "A recipe journal where members of a small group can post recipes with photos. Sign-in by email. Posts get reactions, and the highest-rated of the month gets pinned. Make it feel a bit warm — like a printed cookbook.",
  },
  {
    slug: "photo-album",
    num: "02",
    category: "sharing",
    name: "Photo Album",
    tagline: "Pictures with captions, like a real one.",
    bullets: ["upload & caption", "ordered grid", "private link share"],
    estSeconds: 25,
    defaultPrompt:
      "A photo album. Upload pictures with captions. Pictures stay in upload order. A private share link lets a few friends view (no sign-in for viewers). Layout should feel like flipping through a real album.",
  },
  {
    slug: "reading-room",
    num: "03",
    category: "sharing",
    name: "Reading Room",
    tagline: "A list shared with a few people.",
    bullets: ["book entries", "finished / unfinished", "ratings"],
    estSeconds: 20,
    defaultPrompt:
      "A small private reading list shared with two or three friends. Each entry has a title, author, status (finished / reading / unfinished), and a star rating. Keep it minimal — a typeset list, not a spreadsheet.",
  },
  {
    slug: "newsletter-signup",
    num: "04",
    category: "collecting",
    name: "Newsletter Signup",
    tagline: "An email list with a small archive.",
    bullets: ["email capture", "past-issues page", "confirmation email"],
    estSeconds: 25,
    defaultPrompt:
      "A newsletter signup page. Captures email addresses, sends a confirmation email, and shows past issues on a separate /archive page. Subtle, editorial design — not corporate.",
  },
  {
    slug: "booking",
    num: "05",
    category: "collecting",
    name: "Booking",
    tagline: "An appointment calendar that doesn't suck.",
    bullets: ["calendar slot grid", "email confirm", "cancel / reschedule"],
    estSeconds: 60,
    defaultPrompt:
      "An appointment booking page. Show a calendar with available time slots. Booking takes name + email + reason. Send a confirmation email with a cancel/reschedule link. Calm, uncrowded UI.",
  },
  {
    slug: "survey",
    num: "06",
    category: "collecting",
    name: "Survey",
    tagline: "Ask a question, see the answers.",
    bullets: ["form builder", "response view", "CSV export"],
    estSeconds: 30,
    defaultPrompt:
      "A small survey tool. I (the creator) can define questions; respondents fill in answers anonymously. The /results page shows aggregated answers, with a CSV export button. Clean form layout.",
  },
  {
    slug: "tip-jar",
    num: "07",
    category: "selling",
    name: "Tip Jar",
    tagline: "One-time payments via Stripe.",
    bullets: ["Stripe Checkout", "custom amount", "thank-you page"],
    estSeconds: 60,
    defaultPrompt:
      "A tip jar. People can pay any amount via Stripe Checkout. Show a warm thank-you page with the supporter's name (optional). Stripe keys read from secrets.",
  },
  {
    slug: "subscription",
    num: "08",
    category: "selling",
    name: "Subscription",
    tagline: "Monthly support, with tiers.",
    bullets: ["Stripe recurring", "members-only content", "cancel anytime"],
    estSeconds: 90,
    defaultPrompt:
      "A subscription page with two tiers (monthly $5, monthly $15). Members get access to a /members page with gated content. Cancel-anytime via Stripe customer portal. Editorial, not SaaS-y.",
  },
  {
    slug: "storefront",
    num: "09",
    category: "selling",
    name: "Storefront",
    tagline: "A small shop with cart & checkout.",
    bullets: ["products grid", "shopping cart", "Stripe Checkout"],
    estSeconds: 120,
    defaultPrompt:
      "A small shop. Products have name, price, photo, description, inventory count. Shopping cart on the right. Checkout via Stripe (one-time payments). After purchase, show a thank-you page and email the buyer.",
  },
  {
    slug: "portfolio",
    num: "10",
    category: "showing",
    name: "Portfolio",
    tagline: "Show your work, beautifully.",
    bullets: ["hero + about", "project grid", "contact form"],
    estSeconds: 45,
    defaultPrompt:
      "A portfolio site for a creative person. Hero with a one-line bio and links to social. A project grid with thumbnails (3 example projects). An /about page. A small contact form (no backend — just sends email).",
  },
  {
    slug: "personal-page",
    num: "11",
    category: "showing",
    name: "Personal Page",
    tagline: "One page that's just you.",
    bullets: ["hero + bio", "links", "contact"],
    estSeconds: 20,
    defaultPrompt:
      "A single-page personal site. Big serif hero with my name and a one-paragraph bio. A vertical list of links (twitter, github, email, …). Editorial, slightly italicized, warm.",
  },
  {
    slug: "todo-list",
    num: "12",
    category: "internal",
    name: "Todo List",
    tagline: "A simple list, just for you.",
    bullets: ["add / done / delete", "private to your account", "keyboard shortcuts"],
    estSeconds: 15,
    defaultPrompt:
      "A simple todo list, private to my account (sign-in required). Add, mark done, delete. Keyboard shortcuts: Enter to add, Esc to clear input, Cmd-/ to toggle done on the focused item. Spartan, paper-feeling.",
  },
];

export const TEMPLATE_CATEGORIES: { key: TemplateCategory | "all"; label: string }[] = [
  { key: "all",        label: "all" },
  { key: "sharing",    label: "sharing" },
  { key: "collecting", label: "collecting" },
  { key: "selling",    label: "selling" },
  { key: "showing",    label: "showing" },
  { key: "internal",   label: "internal" },
];

export function getTemplate(slug: string | null | undefined): Template | null {
  if (!slug) return null;
  return TEMPLATES.find((t) => t.slug === slug) ?? null;
}

/** Compact human estimate, e.g. "~ 30 s" or "~ 2 min". */
export function estLabel(s: number): string {
  if (s < 60) return `~ ${s} s`;
  const m = Math.round(s / 60);
  return `~ ${m} min`;
}
