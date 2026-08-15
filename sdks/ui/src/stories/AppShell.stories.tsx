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
 * recipe for a grouped, icon+label navigation rail: a quiet group label,
 * a comfortable rounded hit target with a hover fill, a focus-visible
 * ring, and one accent-tinted active item. Everything is expressed in
 * `--zeroship-*` tokens (no raw hex/px) so the recipe drops straight into a
 * governed surface. Scoped under
 * `.zeroship-demo-nav` so it can't leak past the story.
 */
const navRecipe = `
.zeroship-demo-nav {
  display: flex;
  flex-direction: column;
  gap: var(--zeroship-space-5);
}
.zeroship-demo-nav__group {
  display: flex;
  flex-direction: column;
  gap: var(--zeroship-space-half);
}
.zeroship-demo-nav__label {
  margin: 0;
  padding-block: var(--zeroship-space-1);
  padding-inline: var(--zeroship-space-2);
  /* Secondary (not tertiary) label ink: tertiary fails WCAG contrast over
   * the semi-transparent sunken rail once composited against the page.
   * Secondary keeps the quiet uppercase-caption read while passing AA. */
  color: var(--zeroship-label-secondary);
  font-size: var(--zeroship-text-caption-2-size);
  line-height: var(--zeroship-text-caption-2-line);
  font-weight: var(--zeroship-text-caption-2-weight);
  letter-spacing: 0.08em;
  text-transform: uppercase;
}
.zeroship-demo-nav__item {
  position: relative;
  display: flex;
  align-items: center;
  gap: var(--zeroship-space-2);
  padding-block: var(--zeroship-space-2);
  padding-inline: var(--zeroship-space-2);
  border-radius: var(--zeroship-radius-full);
  color: var(--zeroship-label-secondary);
  font-size: var(--zeroship-text-callout-size);
  line-height: var(--zeroship-text-callout-line);
  font-weight: var(--zeroship-text-callout-weight);
  text-decoration: none;
  transition: background-color var(--zeroship-motion-fast) var(--zeroship-motion-ease),
    color var(--zeroship-motion-fast) var(--zeroship-motion-ease);
}
.zeroship-demo-nav__item:hover {
  background: var(--zeroship-fill-quaternary);
  backdrop-filter: var(--zeroship-control-backdrop-filter, none);
  -webkit-backdrop-filter: var(--zeroship-control-backdrop-filter, none);
  color: var(--zeroship-label);
}
.zeroship-demo-nav__item:focus-visible {
  outline: var(--zeroship-focus-ring-width) solid var(--zeroship-focus-ring-color);
  outline-offset: var(--zeroship-focus-ring-offset);
}
.zeroship-demo-nav__item[aria-current="page"] {
  background: color-mix(in oklch, var(--zeroship-accent) 10%, var(--zeroship-surface));
  backdrop-filter: var(--zeroship-control-backdrop-filter, none);
  -webkit-backdrop-filter: var(--zeroship-control-backdrop-filter, none);
  box-shadow: var(--zeroship-input-surface-edge, none);
  color: var(--zeroship-accent-hover);
  font-weight: var(--zeroship-text-headline-weight);
}
.zeroship-demo-nav__item[aria-current="page"]::before {
  content: none;
}
.zeroship-demo-nav__icon {
  flex: 0 0 auto;
  display: inline-flex;
  inline-size: var(--zeroship-space-4);
  block-size: var(--zeroship-space-4);
}
.zeroship-demo-nav__icon svg {
  inline-size: 100%;
  block-size: 100%;
}
/* Header global-action glyph — sized in tokens like the nav icon so the
 * icon-only ghost button reads as a peer of the avatar. */
.zeroship-demo-header__icon {
  display: inline-flex;
  inline-size: var(--zeroship-space-4);
  block-size: var(--zeroship-space-4);
}
.zeroship-demo-header__icon svg {
  inline-size: 100%;
  block-size: 100%;
}
/* The brand mark — an accent-tinted rounded square holding the glyph. */
.zeroship-demo-brand {
  display: inline-flex;
  align-items: center;
  gap: var(--zeroship-space-2);
}
.zeroship-demo-brand__mark {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  inline-size: var(--zeroship-space-6);
  block-size: var(--zeroship-space-6);
  border-radius: var(--zeroship-radius-full);
  background: color-mix(in oklch, var(--zeroship-accent) 16%, transparent);
  color: var(--zeroship-accent);
}
.zeroship-demo-brand__mark svg {
  inline-size: var(--zeroship-space-4);
  block-size: var(--zeroship-space-4);
}
.zeroship-demo-brand__word {
  font-size: var(--zeroship-text-headline-size);
  line-height: var(--zeroship-text-headline-line);
  font-weight: var(--zeroship-text-headline-weight);
  letter-spacing: var(--zeroship-text-headline-tracking);
  color: var(--zeroship-label);
}
@media (prefers-reduced-motion: reduce) {
  .zeroship-demo-nav__item {
    transition: none;
  }
}
@media (forced-colors: active) {
  .zeroship-demo-nav__item[aria-current="page"] {
    color: Highlight;
  }
  .zeroship-demo-nav__item[aria-current="page"]::before {
    background: Highlight;
  }
}
`;

/* ─── decorative inline icons (all aria-hidden — labels carry meaning) ──── */

type IconProps = { children: ReactNode };
const Icon = ({ children }: IconProps) => (
  <span className="zeroship-demo-nav__icon" aria-hidden="true">
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
    className="zeroship-demo-nav__item"
    aria-current={current ? "page" : undefined}
  >
    {icon}
    {label}
  </a>
);

const PrimaryNav = ({ groups }: { groups: NavGroup[] }) => (
  <div className="zeroship-demo-nav">
    {groups.map((group) => (
      <div className="zeroship-demo-nav__group" key={group.label}>
        <p className="zeroship-demo-nav__label">{group.label}</p>
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
  <span className="zeroship-demo-brand">
    <span className="zeroship-demo-brand__mark" aria-hidden="true">
      <svg viewBox="0 0 24 24" fill="none" aria-hidden="true">
        <path
          d="M12 3 3 19h6l3-6 3 6h6Z"
          fill="currentColor"
          fillOpacity="0.9"
        />
      </svg>
    </span>
    <span className="zeroship-demo-brand__word">Acme</span>
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
      gap: "var(--zeroship-space-3)",
      inlineSize: "100%",
    }}
  >
    {extraStart}
    <BrandLockup />
    <div style={{ flex: 1 }} />
    <Button variant="plain" size="small" aria-label="Notifications">
      <span className="zeroship-demo-header__icon" aria-hidden="true">
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
      gap: "var(--zeroship-space-4)",
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
          // `<main class="zeroship-story-main">`. AppShell legitimately renders
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
              paddingBlock: "var(--zeroship-space-6)",
              display: "flex",
              flexDirection: "column",
              // A step more vertical air (6 → 7) so the stat row breathes
              // away from the activity card and the page reads less dense.
              gap: "var(--zeroship-space-7)",
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
    // decorator wraps the story in its OWN `<main class="zeroship-story-main">`,
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
      '.zeroship-demo-nav__item[aria-current="page"]',
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
          "own state; the rail animates to zero inline-size and is taken " +
          "out of the AT tree via `inert` + `aria-hidden` so Main spans " +
          "full width. The consumer owns the trigger and passes " +
          "`sidebarOpen` / `onSidebarOpenChange`.",
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
            style={{ paddingBlock: "var(--zeroship-space-6)" }}
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
    // Collapsed: the sidebar rail carries inert + aria-hidden → out of the
    // AT tree, so the navigation landmark should not be queryable.
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
            style={{ paddingBlock: "var(--zeroship-space-6)" }}
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
            style={{ paddingBlock: "var(--zeroship-space-6)" }}
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

/* ─── 5. Scroll frame — Main owns the scroll (regression) ───────────────── */
export const ScrollFrame: Story = {
  name: "Scroll frame",
  parameters: {
    docs: {
      description: {
        story:
          "Shows this component behavior with realistic content and keeps the edge case easy to inspect.",
      },
    },
  },
  render: () => (
    // Constrain the shell to a fixed, smallish frame so the tall Main
    // content must scroll WITHIN it rather than growing the page.
    <div style={{ blockSize: "320px" }}>
      <AppShell style={{ blockSize: "100%" }}>
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
            <Container size="lg" style={{ paddingBlock: "var(--zeroship-space-6)" }}>
              {/* Real app main areas carry focusable controls; a leading
                  action makes the scroll region keyboard-reachable so it is
                  not an inaccessible scroll trap (axe
                  scrollable-region-focusable). */}
              <Button variant="filled" size="small">
                New row
              </Button>
              {Array.from({ length: 40 }, (_, i) => (
                <p key={i}>
                  Row {i + 1} —{" "}
                  <a href="#">tall content that overflows the fixed frame</a> so
                  Main must scroll within the shell.
                </p>
              ))}
            </Container>
          </AppShell.Main>
        </AppShell.Body>
        <AppShell.Footer>
          <FooterBar />
        </AppShell.Footer>
      </AppShell>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const main = canvasElement.querySelector<HTMLElement>(
      "main.zs-app-shell__main",
    );
    const header = canvasElement.querySelector<HTMLElement>(
      ".zs-app-shell__header",
    );
    await expect(main).not.toBeNull();
    await expect(header).not.toBeNull();
    // Main is the scroller: its content is taller than its box.
    await expect(main!.scrollHeight).toBeGreaterThan(main!.clientHeight);
    // Header is pinned: scrolling Main does not move the header's top.
    const headerTopBefore = header!.getBoundingClientRect().top;
    main!.scrollTop = main!.scrollHeight;
    const headerTopAfter = header!.getBoundingClientRect().top;
    await expect(Math.abs(headerTopAfter - headerTopBefore)).toBeLessThanOrEqual(
      1,
    );
  },
};
