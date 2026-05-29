import type { Meta, StoryObj } from "@storybook/react";
import { expect, fn, userEvent, within } from "@storybook/test";
import { AppShell, useAppShellSidebar } from "../layouts";
import { Button } from "../components";

// Module-level spy for the controlled-toggle story. Created once (not
// per-render) so re-renders triggered by the click don't swap in a fresh
// zero-call spy before the play() reads it. Cleared at the top of play().
const toggleSpy = fn();

const navLink = (label: string) => (
  <a
    href="#"
    style={{
      display: "block",
      padding: "var(--zs-space-2) var(--zs-space-3)",
      borderRadius: "var(--zs-radius-2)",
      color: "var(--zs-label)",
      textDecoration: "none",
    }}
  >
    {label}
  </a>
);

const meta: Meta<typeof AppShell> = {
  title: "Layouts/AppShell",
  component: AppShell,
  // The shell paints a full-height frame; render it flush so the bands
  // read as a real app chrome rather than inside Storybook's padding.
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

/* ─── 1. Full shell — header + sidebar + main + footer ──────────────────── */
export const FullShell: Story = {
  name: "Full shell (uncontrolled)",
  parameters: {
    docs: {
      description: {
        story:
          "The full app frame: a `<header>` band, a body `Split` (sidebar " +
          "`<nav>` rail + `<main>`), and a `<footer>`. Uncontrolled — the " +
          "shell seeds its own sidebar state from `defaultSidebarOpen` " +
          "(default open). A skip-to-content link is baked in as the first " +
          "focusable child, targeting the Main's generated id.",
      },
    },
  },
  render: () => (
    <AppShell>
      <AppShell.Header>
        <strong>Acme</strong>
      </AppShell.Header>
      <AppShell.Body>
        <AppShell.Sidebar asChild>
          <nav aria-label="Primary">
            {navLink("Dashboard")}
            {navLink("Projects")}
            {navLink("Settings")}
          </nav>
        </AppShell.Sidebar>
        <AppShell.Main>
          <div style={{ padding: "var(--zs-space-6)" }}>
            <h1>Dashboard</h1>
            <p>Welcome back.</p>
          </div>
        </AppShell.Main>
      </AppShell.Body>
      <AppShell.Footer>
        <small>© Acme</small>
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
      'main.zs-app-shell__main',
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
        <strong>Acme</strong>
      </AppShell.Header>
      <AppShell.Body>
        <AppShell.Sidebar asChild>
          <nav aria-label="Primary">{navLink("Dashboard")}</nav>
        </AppShell.Sidebar>
        <AppShell.Main>
          <div style={{ padding: "var(--zs-space-6)" }}>
            <h1>Full-width content</h1>
          </div>
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
        <SidebarToggle />
      </AppShell.Header>
      <AppShell.Body>
        <AppShell.Sidebar asChild>
          <nav aria-label="Primary">{navLink("Dashboard")}</nav>
        </AppShell.Sidebar>
        <AppShell.Main>
          <div style={{ padding: "var(--zs-space-6)" }}>
            <h1>Content</h1>
          </div>
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
      canvasElement.querySelectorAll('main.zs-app-shell__main')
        .length,
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
          "`sidebarSide=\"end\"` puts the rail on the inline-end edge via " +
          "the underlying `Split side=\"end\"` (visual reversal only — the " +
          "DOM/reading order stays sidebar-then-main). Flips under RTL.",
      },
    },
  },
  render: () => (
    <AppShell sidebarSide="end" sidebarWidth="14rem">
      <AppShell.Header>
        <strong>Acme</strong>
      </AppShell.Header>
      <AppShell.Body>
        <AppShell.Sidebar asChild>
          <nav aria-label="Primary">{navLink("Dashboard")}</nav>
        </AppShell.Sidebar>
        <AppShell.Main>
          <div style={{ padding: "var(--zs-space-6)" }}>
            <h1>Content</h1>
          </div>
        </AppShell.Main>
      </AppShell.Body>
    </AppShell>
  ),
};
