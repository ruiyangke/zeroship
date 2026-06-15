import type { Meta, StoryObj } from "@storybook/react";
import { expect, fn, userEvent, within } from "@storybook/test";
import { FormSection } from "../blocks";
import { Field } from "../components/Field";
import { Input } from "../components/Input";
import { Button } from "../components/Button";

const meta: Meta<typeof FormSection> = {
  title: "Blocks/FormSection",
  component: FormSection,
  parameters: { layout: "fullscreen" },
};

export default meta;

type Story = StoryObj<typeof FormSection>;

const shellStyle: React.CSSProperties = {
  inlineSize: "100%",
  maxInlineSize: "56rem",
  boxSizing: "border-box",
  margin: "var(--zs-space-8) auto",
  padding: "0 var(--zs-space-6)",
};

/* A constrained shell so the fullscreen stories read as a real settings
   panel rather than a full-bleed band. */
function Shell({ children }: { children: React.ReactNode }) {
  return <div style={shellStyle}>{children}</div>;
}

/* ─── 1. Stacked (default) ─────────────────────────────────────────────
 * The everyday form section: title + description, a couple of Fields, a
 * Save/Cancel footer. Single column. */
export const Stacked: Story = {
  name: "Stacked (title · fields · save/cancel)",
  parameters: {
    docs: {
      description: {
        story:
          "The ergonomic surface in `stacked` (default) orientation: pass " +
          "`title`/`description`/`footer` props and let `children` be the " +
          "body Fields. The section is named by its heading via " +
          "`aria-labelledby`; the footer is a Separator + a right-aligned " +
          "row of Buttons.",
      },
    },
  },
  render: () => {
    const onSave = fn();
    // Stash so play() can assert the same fn instance fired.
    (window as unknown as Record<string, unknown>).__fsStackedSave = onSave;
    return (
      <Shell>
        <FormSection
          data-testid="fs-stacked"
          title="Profile"
          description="This information is shown on your public profile."
          footer={
            <>
              <Button variant="plain">Cancel</Button>
              <Button data-testid="fs-save" onClick={onSave}>
                Save
              </Button>
            </>
          }
        >
          <Field>
            <Field.Label>Display name</Field.Label>
            <Input defaultValue="Ada Lovelace" />
          </Field>
          <Field>
            <Field.Label>Bio</Field.Label>
            <Input placeholder="A short bio" />
          </Field>
        </FormSection>
      </Shell>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    // The section is labelled by its heading: the root region's
    // accessible name resolves to the title text.
    const region = canvas.getByRole("region", { name: "Profile" });
    await expect(region).toBeInTheDocument();
    await expect(region.tagName).toBe("SECTION");

    // Regression: fullscreen Storybook wraps the story in a centered canvas.
    // The Shell must claim width explicitly so FormSection doesn't collapse
    // under inline-size containment. Compare against the Shell's content box
    // rather than a fixed viewport width so mobile previews remain valid.
    const shell = region.parentElement;
    await expect(shell).toBeTruthy();
    const shellStyle = getComputedStyle(shell as HTMLElement);
    const shellContentWidth =
      (shell as HTMLElement).getBoundingClientRect().width -
      Number.parseFloat(shellStyle.paddingInlineStart || "0") -
      Number.parseFloat(shellStyle.paddingInlineEnd || "0");
    await expect(region.getBoundingClientRect().width).toBeGreaterThan(0);
    await expect(region.getBoundingClientRect().width).toBeGreaterThanOrEqual(
      shellContentWidth - 1,
    );

    // aria-labelledby points at the heading element, and that element is
    // present in the DOM with the title text.
    const labelledby = region.getAttribute("aria-labelledby");
    await expect(labelledby).toBeTruthy();
    const heading = canvas.getByRole("heading", { name: "Profile" });
    await expect(heading.id).toBe(labelledby);
    await expect(heading.tagName).toBe("H3");

    // A footer Button click fires its handler (the stashed fn spy).
    const onSave = (window as unknown as Record<string, unknown>)
      .__fsStackedSave as ReturnType<typeof fn>;
    const save = canvas.getByTestId("fs-save");
    await expect(onSave).not.toHaveBeenCalled();
    await userEvent.click(save);
    await expect(onSave).toHaveBeenCalledTimes(1);
  },
};

/* ─── 2. Aside (two-column settings) ───────────────────────────────────
 * Header in a start column, body in the end column; footer spans under
 * the body. Collapses to stacked below --zs-bp-md. */
export const Aside: Story = {
  name: "Aside (two-column settings)",
  parameters: {
    docs: {
      description: {
        story:
          "`orientation=\"aside\"`: the header sits in a start column and " +
          "the body Fields in the end column — the settings-page two-column " +
          "arrangement. The footer spans the full width under the body. " +
          "Below `--zs-bp-md` (48rem) the layout collapses to stacked.",
      },
    },
  },
  render: () => {
    const onSave = fn();
    (window as unknown as Record<string, unknown>).__fsAsideSave = onSave;
    return (
      <Shell>
        <FormSection
          data-testid="fs-aside"
          orientation="aside"
          title="Notifications"
          description="Choose how and when we contact you."
          footer={
            <>
              <Button variant="plain">Discard</Button>
              <Button data-testid="fs-aside-save" onClick={onSave}>
                Save changes
              </Button>
            </>
          }
        >
          <Field>
            <Field.Label>Notification email</Field.Label>
            <Input type="email" defaultValue="ada@example.com" />
          </Field>
          <Field>
            <Field.Label>Digest frequency</Field.Label>
            <Input defaultValue="Weekly" />
          </Field>
        </FormSection>
      </Shell>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const region = canvas.getByRole("region", { name: "Notifications" });
    await expect(region).toHaveAttribute("data-orientation", "aside");

    const labelledby = region.getAttribute("aria-labelledby");
    const heading = canvas.getByRole("heading", { name: "Notifications" });
    await expect(heading.id).toBe(labelledby);

    const onSave = (window as unknown as Record<string, unknown>)
      .__fsAsideSave as ReturnType<typeof fn>;
    const save = canvas.getByTestId("fs-aside-save");
    await userEvent.click(save);
    await expect(onSave).toHaveBeenCalledTimes(1);
  },
};

/* ─── 3. Compound (the parts form) ─────────────────────────────────────
 * Full control of each region via FormSection.Header/.Body/.Footer, with
 * a relevelled Title (asChild → h2). */
export const Compound: Story = {
  name: "Compound parts (relevelled heading)",
  parameters: {
    docs: {
      description: {
        story:
          "The compound surface: drop `FormSection.Header` / `.Body` / " +
          "`.Footer` into `children` for full control. Here the Title is " +
          "relevelled to an `<h2>` via `asChild` — the section's " +
          "`aria-labelledby` still resolves to it because the shared title " +
          "id flows through context.",
      },
    },
  },
  render: () => {
    const onSave = fn();
    (window as unknown as Record<string, unknown>).__fsCompoundSave = onSave;
    return (
      <Shell>
        <FormSection data-testid="fs-compound">
          <FormSection.Header>
            <FormSection.Title asChild>
              <h2>Security</h2>
            </FormSection.Title>
            <FormSection.Description>
              Manage your password and active sessions.
            </FormSection.Description>
          </FormSection.Header>
          <FormSection.Body>
            <Field>
              <Field.Label>Current password</Field.Label>
              <Input type="password" />
            </Field>
            <Field>
              <Field.Label>New password</Field.Label>
              <Input type="password" />
            </Field>
          </FormSection.Body>
          <FormSection.Footer align="between">
            <Button variant="plain" data-testid="fs-compound-reset">
              Reset
            </Button>
            <Button data-testid="fs-compound-save" onClick={onSave}>
              Update password
            </Button>
          </FormSection.Footer>
        </FormSection>
      </Shell>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    // The relevelled heading is an <h2> and is still the labelledby target.
    const region = canvas.getByRole("region", { name: "Security" });
    const labelledby = region.getAttribute("aria-labelledby");
    const heading = canvas.getByRole("heading", { name: "Security", level: 2 });
    await expect(heading.id).toBe(labelledby);

    const onSave = (window as unknown as Record<string, unknown>)
      .__fsCompoundSave as ReturnType<typeof fn>;
    const save = canvas.getByTestId("fs-compound-save");
    await userEvent.click(save);
    await expect(onSave).toHaveBeenCalledTimes(1);
  },
};

/* ─── 4. CompoundPartsInFragment (regression) ─────────────────────────
 * Compound parts wrapped in a React Fragment (`<>…</>`) as children must
 * still be detected as compound, so the implicit Body wrapper is NOT
 * applied. Pre-fix the displayName scan used
 * `Array.isArray(children) ? children : [children]`, which does not
 * flatten a Fragment — so `usingCompound` was false and the whole
 * Fragment got wrapped in an implicit <FormSectionBody>, nesting the
 * Header inside the body region and breaking the grid-area layout. */
export const CompoundPartsInFragment: Story = {
  name: "Compound parts inside a Fragment",
  parameters: {
    docs: {
      description: {
        story:
          "Compound parts wrapped in a React Fragment as `children`. The " +
          "Fragment is flattened (via `React.Children.toArray`) so the " +
          "Header/Body/Footer are detected as the compound surface: the " +
          "Header renders in the header region and is NOT nested inside the " +
          "implicit body slot.",
      },
    },
  },
  render: () => (
    <Shell>
      <FormSection data-testid="fs-fragment">
        <>
          <FormSection.Header>
            {/* Releveled to <h2> (like the Security story) so this isolated
                story is heading-order-clean; the Fragment-flattening behavior
                under test is unaffected by the heading level. */}
            <FormSection.Title asChild>
              <h2>Billing</h2>
            </FormSection.Title>
            <FormSection.Description>
              Manage your plan and payment method.
            </FormSection.Description>
          </FormSection.Header>
          <FormSection.Body>
            <Field>
              <Field.Label>Card number</Field.Label>
              <Input defaultValue="•••• •••• •••• 4242" readOnly />
            </Field>
          </FormSection.Body>
          <FormSection.Footer>
            <Button>Update plan</Button>
          </FormSection.Footer>
        </>
      </FormSection>
    </Shell>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    const region = canvas.getByRole("region", { name: "Billing" });

    // The Header region exists and is NOT a descendant of the body slot
    // (pre-fix the Fragment was double-wrapped in the implicit body).
    const header = region.querySelector<HTMLElement>(
      '[data-slot="form-section-header"]',
    );
    const body = region.querySelector<HTMLElement>(
      '[data-slot="form-section-body"]',
    );
    await expect(header).not.toBeNull();
    await expect(body).not.toBeNull();
    await expect(body!.contains(header)).toBe(false);

    // Exactly one body slot (no implicit Body wrapping the explicit one).
    await expect(
      region.querySelectorAll('[data-slot="form-section-body"]'),
    ).toHaveLength(1);

    // The heading still resolves the labelledby relationship.
    const labelledby = region.getAttribute("aria-labelledby");
    const heading = canvas.getByRole("heading", { name: "Billing" });
    await expect(heading.id).toBe(labelledby);
  },
};

/* ─── 5. NoFooter ──────────────────────────────────────────────────────
 * A footerless section — just header + body. */
export const NoFooter: Story = {
  name: "No footer (header + body only)",
  parameters: {
    docs: {
      description: {
        story:
          "Omit the `footer` prop (and any `FormSection.Footer`) for a " +
          "read-only / no-actions section. The section is still labelled by " +
          "its heading.",
      },
    },
  },
  render: () => (
    <Shell>
      <FormSection
        data-testid="fs-nofooter"
        title="Account"
        description="Your account identifiers (read-only)."
      >
        <Field>
          <Field.Label>Account ID</Field.Label>
          <Input defaultValue="acct_1a2b3c" readOnly />
        </Field>
        <Field>
          <Field.Label>Created</Field.Label>
          <Input defaultValue="2026-01-14" readOnly />
        </Field>
      </FormSection>
    </Shell>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const region = canvas.getByRole("region", { name: "Account" });
    const labelledby = region.getAttribute("aria-labelledby");
    const heading = canvas.getByRole("heading", { name: "Account" });
    await expect(heading.id).toBe(labelledby);

    // No footer region rendered.
    await expect(
      region.querySelector('[data-slot="form-section-footer"]'),
    ).toBeNull();
  },
};
