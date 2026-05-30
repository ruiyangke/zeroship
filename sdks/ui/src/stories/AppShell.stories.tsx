import type { Meta, StoryObj } from "@storybook/react";
import { expect, fn, userEvent, within } from "@storybook/test";
import type { ReactNode } from "react";
import {
  AppShell,
  Container,
  Grid,
  PageHeader,
  useAppShellSidebar,
} from "../layouts";
import { Avatar, Button, Card } from "../components";
import { DescriptionList, StatCard } from "../blocks";

/* ─── nav-item recipe (the recipe consumers copy) ─────────────────────────
 *
 * The AppShell stays STRUCTURAL — it ships the rail surface + divider but
 * no opinion on what a nav item looks like. These styles are the showcase
 * recipe for a grouped, icon+label navigation rail: a sunken-rail group
 * label, a comfortable hit-target link with a hover fill, a focus-visible
 * ring, and one accent-tinted ACTIVE item carrying a leading accent
 * indicator. Everything is expressed in `--zs-*` tokens (no raw hex/px) so
 * the recipe drops straight into a governed surface. Scoped under
 * `.zs-demo-nav` so it can't leak past the story.
 */
const navRecipe = `
.zs-demo-nav {
  display: flex;
  flex-direction: column;
  gap: var(--zs-space-5);
}
.zs-demo-nav__group {
  display: flex;
  flex-direction: column;
  gap: var(--zs-space-half);
}
.zs-demo-nav__label {
  margin: 0;
  padding-block: var(--zs-space-1);
  padding-inline: var(--zs-space-2);
  /* Secondary (not tertiary) label ink: tertiary fails WCAG contrast over
   * the semi-transparent sunken rail once composited against the page.
   * Secondary keeps the quiet uppercase-caption read while passing AA. */
  color: var(--zs-label-secondary);
  font-size: var(--zs-text-caption-2-size);
  line-height: var(--zs-text-caption-2-line);
  font-weight: var(--zs-text-caption-2-weight);
  letter-spacing: 0.08em;
  text-transform: uppercase;
}
.zs-demo-nav__item {
  position: relative;
  display: flex;
  align-items: center;
  gap: var(--zs-space-2);
  padding-block: var(--zs-space-2);
  padding-inline: var(--zs-space-2);
  border-radius: var(--zs-radius-2);
  color: var(--zs-label-secondary);
  font-size: var(--zs-text-callout-size);
  line-height: var(--zs-text-callout-line);
  font-weight: var(--zs-text-callout-weight);
  text-decoration: none;
  transition: background-color var(--zs-motion-fast) var(--zs-motion-ease),
    color var(--zs-motion-fast) var(--zs-motion-ease);
}
.zs-demo-nav__item:hover {
  background: var(--zs-fill-quaternary);
  color: var(--zs-label);
}
.zs-demo-nav__item:focus-visible {
  outline: var(--zs-focus-ring-width) solid var(--zs-focus-ring-color);
  outline-offset: var(--zs-focus-ring-offset);
}
/* Active item — the leading accent RAIL is the one dominant signal; a
 * light accent fill + a calmer accent ink support it without shouting.
 * The indicator is an inline-start border so it tracks the reading edge
 * under RTL automatically. */
.zs-demo-nav__item[aria-current="page"] {
  /* Lighter fill (8%, down from 14%) so the rail — not the tint — leads. */
  background: color-mix(in oklch, var(--zs-accent) 8%, transparent);
  /* The calmer accent-hover step (vs the louder accent-active) still
   * clears WCAG AA on the lighter 8% fill (measured ~5.5:1), so the
   * active item stays legible while reading quieter. The leading rail +
   * heavier weight carry the active state without relying on color alone. */
  color: var(--zs-accent-hover);
  font-weight: var(--zs-text-headline-weight);
}
.zs-demo-nav__item[aria-current="page"]::before {
  content: "";
  position: absolute;
  inset-block: var(--zs-space-1);
  inset-inline-start: 0;
  inline-size: var(--zs-space-half);
  border-radius: var(--zs-radius-full);
  background: var(--zs-accent);
}
.zs-demo-nav__icon {
  flex: 0 0 auto;
  display: inline-flex;
  inline-size: var(--zs-space-4);
  block-size: var(--zs-space-4);
}
.zs-demo-nav__icon svg {
  inline-size: 100%;
  block-size: 100%;
}
/* Header global-action glyph — sized in tokens like the nav icon so the
 * icon-only ghost button reads as a peer of the avatar. */
.zs-demo-header__icon {
  display: inline-flex;
  inline-size: var(--zs-space-4);
  block-size: var(--zs-space-4);
}
.zs-demo-header__icon svg {
  inline-size: 100%;
  block-size: 100%;
}
/* The brand mark — an accent-tinted rounded square holding the glyph. */
.zs-demo-brand {
  display: inline-flex;
  align-items: center;
  gap: var(--zs-space-2);
}
.zs-demo-brand__mark {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  inline-size: var(--zs-space-6);
  block-size: var(--zs-space-6);
  border-radius: var(--zs-radius-2);
  background: color-mix(in oklch, var(--zs-accent) 16%, transparent);
  color: var(--zs-accent);
}
.zs-demo-brand__mark svg {
  inline-size: var(--zs-space-4);
  block-size: var(--zs-space-4);
}
.zs-demo-brand__word {
  font-size: var(--zs-text-headline-size);
  line-height: var(--zs-text-headline-line);
  font-weight: var(--zs-text-headline-weight);
  letter-spacing: var(--zs-text-headline-tracking);
  color: var(--zs-label);
}
@media (prefers-reduced-motion: reduce) {
  .zs-demo-nav__item {
    transition: none;
  }
}
@media (forced-colors: active) {
  .zs-demo-nav__item[aria-current="page"] {
    color: Highlight;
  }
  .zs-demo-nav__item[aria-current="page"]::before {
    background: Highlight;
  }
}
`;

/* ─── decorative inline icons (all aria-hidden — labels carry meaning) ──── */

type IconProps = { children: ReactNode };
const Icon = ({ children }: IconProps) => (
  <span className="zs-demo-nav__icon" aria-hidden="true">
    <svg
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.8"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      {children}
    </svg>
  </span>
);

const icons = {
  dashboard: (
    <Icon>
      <rect x="3" y="3" width="7" height="9" rx="1" />
      <rect x="14" y="3" width="7" height="5" rx="1" />
      <rect x="14" y="12" width="7" height="9" rx="1" />
      <rect x="3" y="16" width="7" height="5" rx="1" />
    </Icon>
  ),
  projects: (
    <Icon>
      <path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2Z" />
    </Icon>
  ),
  deployments: (
    <Icon>
      <path d="M12 2 4 6v6c0 5 3.5 8 8 10 4.5-2 8-5 8-10V6Z" />
      <path d="m9 12 2 2 4-4" />
    </Icon>
  ),
  activity: (
    <Icon>
      <path d="M3 12h4l2 6 4-14 2 8h6" />
    </Icon>
  ),
  settings: (
    <Icon>
      <circle cx="12" cy="12" r="3" />
      <path d="M12 2v3M12 19v3M4.2 4.2l2.1 2.1M17.7 17.7l2.1 2.1M2 12h3M19 12h3M4.2 19.8l2.1-2.1M17.7 6.3l2.1-2.1" />
    </Icon>
  ),
  billing: (
    <Icon>
      <rect x="2" y="5" width="20" height="14" rx="2" />
      <path d="M2 10h20" />
    </Icon>
  ),
};

/* ─── nav model + renderer (shared across all stories) ────────────────────
 *
 * `current` flags the single active item; it renders with
 * `aria-current="page"` + the accent recipe. Decorative icons are
 * aria-hidden; the label text carries the accessible name. */
interface NavItem {
  label: string;
  icon: ReactNode;
  current?: boolean;
}
interface NavGroup {
  label: string;
  items: NavItem[];
}

const PRIMARY_NAV: NavGroup[] = [
  {
    label: "Workspace",
    items: [
      { label: "Dashboard", icon: icons.dashboard, current: true },
      { label: "Projects", icon: icons.projects },
      { label: "Deployments", icon: icons.deployments },
      { label: "Activity", icon: icons.activity },
    ],
  },
  {
    label: "Account",
    items: [
      { label: "Settings", icon: icons.settings },
      { label: "Billing", icon: icons.billing },
    ],
  },
];

const NavLink = ({ label, icon, current }: NavItem) => (
  <a
    href="#"
    className="zs-demo-nav__item"
    aria-current={current ? "page" : undefined}
  >
    {icon}
    {label}
  </a>
);

const PrimaryNav = ({ groups }: { groups: NavGroup[] }) => (
  <div className="zs-demo-nav">
    {groups.map((group) => (
      <div className="zs-demo-nav__group" key={group.label}>
        <p className="zs-demo-nav__label">{group.label}</p>
        {group.items.map((item) => (
          <NavLink key={item.label} {...item} />
        ))}
      </div>
    ))}
  </div>
);

/* The brand lockup: an accent-tinted rounded mark (decorative SVG glyph,
 * aria-hidden — the wordmark conveys identity) + the "Acme" wordmark. */
const BrandLockup = () => (
  <span className="zs-demo-brand">
    <span className="zs-demo-brand__mark" aria-hidden="true">
      <svg viewBox="0 0 24 24" fill="none" aria-hidden="true">
        <path
          d="M12 3 3 19h6l3-6 3 6h6Z"
          fill="currentColor"
          fillOpacity="0.9"
        />
      </svg>
    </span>
    <span className="zs-demo-brand__word">Acme</span>
  </span>
);

/* The header row: brand at the start, flexible spacer, then a GLOBAL
 * (workspace/account-scoped) control + the signed-in avatar at the end.
 * Shared by every story so the bar reads consistent. `extraStart` lets the
 * toggle story slot a button before the brand.
 *
 * The header is the workspace/account zone — brand · [global action] ·
 * avatar. Page-scoped actions (e.g. the primary "New project" CTA) live in
 * the PageHeader, not here, so the two zones don't duplicate each other.
 * The global control is an icon-only ghost Button (a notifications glyph,
 * `aria-hidden`) carrying an `aria-label` for its accessible name. */
const HeaderBar = ({ extraStart }: { extraStart?: ReactNode }) => (
  <div
    style={{
      display: "flex",
      alignItems: "center",
      gap: "var(--zs-space-3)",
      inlineSize: "100%",
    }}
  >
    {extraStart}
    <BrandLockup />
    <div style={{ flex: 1 }} />
    <Button variant="plain" size="small" aria-label="Notifications">
      <span className="zs-demo-header__icon" aria-hidden="true">
        <svg
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.8"
          strokeLinecap="round"
          strokeLinejoin="round"
        >
          <path d="M18 8a6 6 0 1 0-12 0c0 7-3 9-3 9h18s-3-2-3-9" />
          <path d="M13.7 21a2 2 0 0 1-3.4 0" />
        </svg>
      </span>
    </Button>
    {/* Fallback-only avatar: the initials paint, and an explicit
        role="img" + aria-label gives the lockup a descriptive accessible
        name ("Ada Lovelace") rather than announcing the bare "AL". */}
    <Avatar
      size="sm"
      fallback="AL"
      role="img"
      aria-label="Ada Lovelace"
    />
  </div>
);

/* The footer: a muted copyright + a couple of footnote links. The shell
 * footer band already centers + mutes; these links just inherit. */
const FooterBar = () => (
  <div
    style={{
      display: "flex",
      alignItems: "center",
      gap: "var(--zs-space-4)",
    }}
  >
    <span>© Acme</span>
    <a href="#" style={{ color: "inherit" }}>
      Privacy
    </a>
    <a href="#" style={{ color: "inherit" }}>
      Status
    </a>
  </div>
);

const meta: Meta<typeof AppShell> = {
  title: "Layouts/AppShell",
  component: AppShell,
  // The shell paints a full-height frame; render it flush so the bands
  // read as a real app chrome rather than inside Storybook's padding.
  // The nav-item recipe `<style>` is injected once per story via the
  // decorator so the rail navigation reads polished.
  decorators: [
    (Story) => (
      <>
        <style>{navRecipe}</style>
        <Story />
      </>
    ),
  ],
  parameters: {
    layout: "fullscreen",
    a11y: {
      config: {
        rules: [
          // The global preview decorator wraps EVERY story in
          // `<main class="zs-story-main">`. AppShell legitimately renders
          // its own `<main>` landmark, so inside the decorator there are
          // two mains and the shell's main is not top-level — both are
          // artifacts of the Storybook harness, not the component. In a
          // real document the shell's `<main>` is the single, top-level
          // main. The `play()` independently asserts exactly one `<main>`
          // inside the component subtree, so the real invariant is still
          // checked. Disable only these two decorator-induced rules.
          { id: "landmark-no-duplicate-main", enabled: false },
          { id: "landmark-main-is-top-level", enabled: false },
        ],
      },
    },
  },
};

export default meta;

type Story = StoryObj<typeof AppShell>;

/* ─── 1. Full shell — a realistic operations console ────────────────────── */
export const FullShell: Story = {
  name: "Full shell (uncontrolled)",
  parameters: {
    docs: {
      description: {
        story:
          "The full app frame as a realistic operations console: a raised " +
          "`<header>` bar (brand lockup + `New project` action + avatar), a " +
          "sunken-rail body `Split` (a grouped `<nav>` with one active item " +
          "+ `<main>`), and a muted `<footer>`. Main wraps its content in a " +
          "`Container` and composes `PageHeader` + a `Grid` of `StatCard`s + " +
          "a `Card` with a `DescriptionList`. Uncontrolled — the shell seeds " +
          "its own sidebar state from `defaultSidebarOpen` (default open). A " +
          "skip-to-content link is baked in as the first focusable child.",
      },
    },
  },
  render: () => (
    <AppShell>
      <AppShell.Header>
        <HeaderBar />
      </AppShell.Header>
      <AppShell.Body>
        <AppShell.Sidebar asChild>
          <nav aria-label="Primary">
            <PrimaryNav groups={PRIMARY_NAV} />
          </nav>
        </AppShell.Sidebar>
        <AppShell.Main>
          <Container
            size="lg"
            style={{
              paddingBlock: "var(--zs-space-6)",
              display: "flex",
              flexDirection: "column",
              // A step more vertical air (6 → 7) so the stat row breathes
              // away from the activity card and the page reads less dense.
              gap: "var(--zs-space-7)",
            }}
          >
            <PageHeader>
              <PageHeader.Text>
                <PageHeader.Breadcrumbs
                  items={[
                    { label: "Home", href: "#" },
                    { label: "Dashboard", current: true },
                  ]}
                />
                <PageHeader.Title>Dashboard</PageHeader.Title>
                <PageHeader.Description>
                  Your workspace at a glance — revenue, active users, and the
                  latest deploys.
                </PageHeader.Description>
              </PageHeader.Text>
              <PageHeader.Actions>
                <Button variant="filled" size="small">
                  New project
                </Button>
              </PageHeader.Actions>
            </PageHeader>

            {/* Flatter `surface` variant (not StatCard's `elevated`
                default — demo override only) so the stat row matches the
                Recent-activity card's weight and doesn't out-prominence
                the main content. */}
            <Grid columns={{ sm: 1, md: 3 }} gap={4}>
              <StatCard
                variant="surface"
                label="Monthly revenue"
                value="$48,120"
                delta={{ value: "12.4%", direction: "up" }}
              />
              <StatCard
                variant="surface"
                label="Active users"
                value="3,204"
                delta={{ value: "5.1%", direction: "up" }}
              />
              <StatCard
                variant="surface"
                label="Deploys this week"
                value="38"
                delta={{ value: "2.0%", direction: "down" }}
              />
            </Grid>

            <Card variant="surface" size="md">
              <Card.Header>
                {/* Relevel to <h2> so the outline reads h1 (page) → h2
                    (card), keeping heading order correct (Card.Title
                    defaults to <h3>). */}
                <Card.Title asChild>
                  <h2>Recent activity</h2>
                </Card.Title>
                <Card.Description>
                  The latest changes across your workspace.
                </Card.Description>
              </Card.Header>
              <Card.Content>
                <DescriptionList divider>
                  <DescriptionList.Item>
                    <DescriptionList.Term>storefront</DescriptionList.Term>
                    <DescriptionList.Detail>
                      Deployed v2.8.0 to production · 14 min ago
                    </DescriptionList.Detail>
                  </DescriptionList.Item>
                  <DescriptionList.Item>
                    <DescriptionList.Term>billing-api</DescriptionList.Term>
                    <DescriptionList.Detail>
                      Merged “Add usage metering” · 1 hr ago
                    </DescriptionList.Detail>
                  </DescriptionList.Item>
                  <DescriptionList.Item>
                    <DescriptionList.Term>docs-site</DescriptionList.Term>
                    <DescriptionList.Detail>
                      Preview build passed checks · 3 hrs ago
                    </DescriptionList.Detail>
                  </DescriptionList.Item>
                </DescriptionList>
              </Card.Content>
            </Card>
          </Container>
        </AppShell.Main>
      </AppShell.Body>
      <AppShell.Footer>
        <FooterBar />
      </AppShell.Footer>
    </AppShell>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Exactly ONE shell <main> — the shell's own. (The global preview
    // decorator wraps the story in its OWN `<main class="zs-story-main">`,
    // a harness artifact; we scope to the shell's `data-slot` to assert
    // the SHELL renders a single main landmark.)
    const shellMains = canvasElement.querySelectorAll(
      "main.zs-app-shell__main",
    );
    await expect(shellMains.length).toBe(1);
    const main = shellMains[0] as HTMLElement;
    // The baked-in skip link targets the shell Main's id.
    const skip = canvasElement.querySelector<HTMLAnchorElement>(
      "a.zs-skip-link",
    );
    await expect(skip).not.toBeNull();
    await expect(main.id).toBeTruthy();
    await expect(skip!.getAttribute("href")).toBe(`#${main.id}`);
    // Composed parts own their semantic data-slot vocabulary (the
    // primitives honor a consumer data-slot; pre-fix Main read
    // "split-main"). Regression guard for the data-slot-override fix.
    await expect(main.getAttribute("data-slot")).toBe("app-shell-main");
    await expect(
      canvasElement.querySelector('[data-slot="app-shell-body"]'),
    ).not.toBeNull();
    await expect(
      canvasElement.querySelector('[data-slot="app-shell-sidebar"]'),
    ).not.toBeNull();
    // Sidebar nav landmark is present.
    await expect(
      canvas.getByRole("navigation", { name: /primary/i }),
    ).toBeInTheDocument();
    // Exactly one active nav item, carrying aria-current="page".
    const active = canvasElement.querySelectorAll(
      '.zs-demo-nav__item[aria-current="page"]',
    );
    await expect(active.length).toBe(1);
  },
};

/* ─── 2. Sidebar collapsed — controlled sidebarOpen={false} ─────────────── */
export const SidebarCollapsed: Story = {
  name: "Sidebar collapsed (controlled)",
  parameters: {
    docs: {
      description: {
        story:
          "Controlled `sidebarOpen={false}`: the shell does not track its " +
          "own state; the rail collapses out of the layout (and the AT " +
          "tree) so Main spans full width. The consumer owns the trigger " +
          "and passes `sidebarOpen` / `onSidebarOpenChange`.",
      },
    },
  },
  render: () => (
    <AppShell sidebarOpen={false}>
      <AppShell.Header>
        <HeaderBar />
      </AppShell.Header>
      <AppShell.Body>
        <AppShell.Sidebar asChild>
          <nav aria-label="Primary">
            <PrimaryNav groups={PRIMARY_NAV} />
          </nav>
        </AppShell.Sidebar>
        <AppShell.Main>
          <Container
            size="lg"
            style={{ paddingBlock: "var(--zs-space-6)" }}
          >
            <PageHeader>
              <PageHeader.Text>
                <PageHeader.Title>Full-width content</PageHeader.Title>
                <PageHeader.Description>
                  With the rail collapsed, Main spans the entire frame.
                </PageHeader.Description>
              </PageHeader.Text>
            </PageHeader>
          </Container>
        </AppShell.Main>
      </AppShell.Body>
    </AppShell>
  ),
  play: async ({ canvasElement }) => {
    // Collapsed: the sidebar rail is display:none → not in the AT tree, so
    // the navigation landmark should not be queryable.
    const canvas = within(canvasElement);
    await expect(
      canvas.queryByRole("navigation", { name: /primary/i }),
    ).toBeNull();
  },
};

/* ─── 3. Toggle round-trip — consumer-owned trigger ─────────────────────── */
// Module-level spy for the controlled-toggle story. Created once (not
// per-render) so re-renders triggered by the click don't swap in a fresh
// zero-call spy before the play() reads it. Cleared at the top of play().
const toggleSpy = fn();

// The toggle lives INSIDE the shell subtree and drives the sidebar via
// `useAppShellSidebar()` — the shell's own state + setter. Calling the
// setter is what fires `onSidebarOpenChange` (the controlled/uncontrolled
// contract: AppShell exposes state + setter, the consumer owns the
// control). This is the documented uncontrolled path; the hook reads the
// current state so the button's aria-expanded reflects it.
function SidebarToggle() {
  const { sidebarOpen, setSidebarOpen } = useAppShellSidebar();
  return (
    <Button
      variant="gray"
      size="small"
      aria-expanded={sidebarOpen}
      onClick={() => setSidebarOpen(!sidebarOpen)}
    >
      Toggle sidebar
    </Button>
  );
}

function ControlledShell({
  onChange,
}: {
  onChange: (open: boolean) => void;
}) {
  return (
    <AppShell defaultSidebarOpen onSidebarOpenChange={onChange}>
      <AppShell.Header>
        <HeaderBar extraStart={<SidebarToggle />} />
      </AppShell.Header>
      <AppShell.Body>
        <AppShell.Sidebar asChild>
          <nav aria-label="Primary">
            <PrimaryNav groups={PRIMARY_NAV} />
          </nav>
        </AppShell.Sidebar>
        <AppShell.Main>
          <Container
            size="lg"
            style={{ paddingBlock: "var(--zs-space-6)" }}
          >
            <PageHeader>
              <PageHeader.Text>
                <PageHeader.Title>Dashboard</PageHeader.Title>
                <PageHeader.Description>
                  Toggle the rail from the header button.
                </PageHeader.Description>
              </PageHeader.Text>
            </PageHeader>
          </Container>
        </AppShell.Main>
      </AppShell.Body>
    </AppShell>
  );
}

export const ToggleRoundTrip: Story = {
  name: "Toggle round-trip",
  parameters: {
    docs: {
      description: {
        story:
          "The toggle is the CONSUMER's responsibility — there is no " +
          "built-in hamburger. Here a `<Button>` rendered inside the shell " +
          "drives the sidebar via `useAppShellSidebar().setSidebarOpen` " +
          "(the shell's own state + setter, uncontrolled from " +
          "`defaultSidebarOpen`). The `play()` clicks it and asserts the " +
          "sidebar hides/shows and `onSidebarOpenChange` fires both ways.",
      },
    },
  },
  render: () => <ControlledShell onChange={toggleSpy} />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Reset the module-level spy so prior runs don't leak call counts.
    toggleSpy.mockClear();
    const spy = toggleSpy;
    const toggle = canvas.getByRole("button", { name: /toggle sidebar/i });
    // Open initially: nav landmark present, exactly one shell <main>.
    await expect(
      canvasElement.querySelectorAll("main.zs-app-shell__main").length,
    ).toBe(1);
    await expect(
      canvas.getByRole("navigation", { name: /primary/i }),
    ).toBeInTheDocument();

    // Click to collapse → rail leaves the AT tree.
    await userEvent.click(toggle);
    await expect(
      canvas.queryByRole("navigation", { name: /primary/i }),
    ).toBeNull();
    await expect(toggle).toHaveAttribute("aria-expanded", "false");
    // onSidebarOpenChange fired with `false`.
    await expect(spy).toHaveBeenCalledWith(false);

    // Click again to re-open.
    await userEvent.click(toggle);
    await expect(
      canvas.getByRole("navigation", { name: /primary/i }),
    ).toBeInTheDocument();
    await expect(toggle).toHaveAttribute("aria-expanded", "true");
    // onSidebarOpenChange fired with `true`.
    await expect(spy).toHaveBeenCalledWith(true);
  },
};

/* ─── 4. Sidebar on the end edge ────────────────────────────────────────── */
export const SidebarEnd: Story = {
  name: "Sidebar on end edge",
  parameters: {
    docs: {
      description: {
        story:
          '`sidebarSide="end"` puts the rail on the inline-end edge via ' +
          'the underlying `Split side="end"` (visual reversal only — the ' +
          "DOM/reading order stays sidebar-then-main). The dividing hairline " +
          "moves to the rail's inline-start edge to keep facing Main. Flips " +
          "under RTL.",
      },
    },
  },
  render: () => (
    <AppShell sidebarSide="end" sidebarWidth="14rem">
      <AppShell.Header>
        <HeaderBar />
      </AppShell.Header>
      <AppShell.Body>
        <AppShell.Sidebar asChild>
          <nav aria-label="Primary">
            <PrimaryNav groups={PRIMARY_NAV} />
          </nav>
        </AppShell.Sidebar>
        <AppShell.Main>
          <Container
            size="lg"
            style={{ paddingBlock: "var(--zs-space-6)" }}
          >
            <PageHeader>
              <PageHeader.Text>
                <PageHeader.Title>Dashboard</PageHeader.Title>
                <PageHeader.Description>
                  The rail sits on the inline-end edge.
                </PageHeader.Description>
              </PageHeader.Text>
            </PageHeader>
          </Container>
        </AppShell.Main>
      </AppShell.Body>
      <AppShell.Footer>
        <FooterBar />
      </AppShell.Footer>
    </AppShell>
  ),
};
