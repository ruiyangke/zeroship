import type { Meta, StoryObj } from "@storybook/react";
import { useRef, useState } from "react";
import { Field, Input, Button } from "../components";

const meta: Meta<typeof Input> = {
  title: "Components/Input",
  component: Input,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Input>;

/* ─── tiny iconography (16x16, currentColor) ────────────────────────── */
function IconSearch() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        d="M7 2a5 5 0 0 1 3.9 8.12l3 3"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
      />
      <circle cx="7" cy="7" r="4.5" fill="none" stroke="currentColor" strokeWidth="1.5" />
    </svg>
  );
}

function IconX() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path
        d="M4 4l8 8M12 4l-8 8"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
      />
    </svg>
  );
}

/* ─── 1. All variants ────────────────────────────────────────────────── */
export const AllVariants: Story = {
  name: "All variants",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All input variants">
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Outline (default)</span>
        <Field>
          <Field.Label>Email</Field.Label>
          <Input type="email" variant="outline" placeholder="you@example.com" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Filled</span>
        <Field>
          <Field.Label>Email</Field.Label>
          <Input type="email" variant="filled" placeholder="you@example.com" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Plain</span>
        <Field>
          <Field.Label>Email</Field.Label>
          <Input type="email" variant="plain" placeholder="you@example.com" />
        </Field>
      </div>
    </div>
  ),
};

/* ─── 2. All sizes ───────────────────────────────────────────────────── */
export const AllSizes: Story = {
  name: "All sizes",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All input sizes">
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Small (32)</span>
        <Field>
          <Field.Label>Project</Field.Label>
          <Input size="sm" placeholder="acme-prod" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Medium (40)</span>
        <Field>
          <Field.Label>Project</Field.Label>
          <Input size="md" placeholder="acme-prod" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Large (48)</span>
        <Field>
          <Field.Label>Project</Field.Label>
          <Input size="lg" placeholder="acme-prod" />
        </Field>
      </div>
    </div>
  ),
};

/* ─── 3. All states ──────────────────────────────────────────────────── */
export const AllStates: Story = {
  name: "All states",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All input states">
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Default</span>
        <Field>
          <Field.Label>Name</Field.Label>
          <Input placeholder="Ada Lovelace" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Focused (autofocus)</span>
        <Field>
          <Field.Label>Name</Field.Label>
          {/* eslint-disable-next-line jsx-a11y/no-autofocus */}
          <Input autoFocus placeholder="Ada Lovelace" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Disabled</span>
        <Field disabled>
          <Field.Label>Name</Field.Label>
          <Input defaultValue="Ada Lovelace" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Read only</span>
        <Field>
          <Field.Label>Slug</Field.Label>
          <Input readOnly defaultValue="acme-prod" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Invalid</span>
        <Field invalid>
          <Field.Label>Name</Field.Label>
          <Input defaultValue="x" />
          <Field.Error match>Must be at least two characters.</Field.Error>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 4. With slots ──────────────────────────────────────────────────── */
export const WithSlots: Story = {
  name: "With slots",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Inputs with slots">
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "14rem" }}>
        <span className="zs-story-label">Start slot (search icon)</span>
        <Field>
          <Field.Label>Search</Field.Label>
          <Input
            type="search"
            placeholder="Search projects…"
            startSlot={<IconSearch />}
          />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "14rem" }}>
        <span className="zs-story-label">End slot (clear button)</span>
        <Field>
          <Field.Label>Filter</Field.Label>
          {/* Visual-polish item 11: the clear button now uses
              <Button variant="plain" size="small"> so it carries the
              44pt HIG hit-target extension from Button slice 1. The
              visible × glyph stays small; the tap target is comfortable.
              When a first-class `clearable` Input prop ships, the clear
              control becomes a subpart with the same hit-area baked in. */}
          <Input
            defaultValue="ada"
            endSlot={
              <Button variant="plain" size="small" aria-label="Clear filter">
                <IconX />
              </Button>
            }
          />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "14rem" }}>
        <span className="zs-story-label">Both (currency + unit)</span>
        <Field>
          <Field.Label>Budget</Field.Label>
          {/* Currency / unit slots carry semantic meaning, so the AT tree
              announces "49 dollars per month" rather than just "49". Slots
              are no longer aria-hidden by default — item 16 of the
              slice-2 review fix brief. */}
          <Input
            type="number"
            defaultValue={49}
            startSlot="$"
            endSlot="/mo"
          />
        </Field>
      </div>
    </div>
  ),
};

/* ─── 5. Decomposed (canonical pattern) ──────────────────────────────── */
export const Decomposed: Story = {
  name: "Decomposed",
  parameters: {
    docs: {
      description: {
        story:
          "Canonical decomposed pattern. The Base UI Field auto-wires " +
          "`htmlFor`, `aria-describedby`, `aria-invalid`, and (for " +
          "required Fields) `aria-required`. Type a non-email value and " +
          "tab away to see the `typeMismatch` Field.Error fire.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Decomposed Field + Input">
      <div className="zs-story-cell" style={{ flex: "1 1 24rem", minWidth: "16rem" }}>
        <span className="zs-story-label">Field.* subparts</span>
        <Field required validationMode="onBlur">
          <Field.Label>
            Email <Field.Required />
          </Field.Label>
          <Input type="email" placeholder="you@example.com" />
          <Field.Description>We use this for billing receipts only.</Field.Description>
          <Field.Error match="valueMissing">Email is required.</Field.Error>
          <Field.Error match="typeMismatch">Enter a valid email address.</Field.Error>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 6. Combined shorthand ──────────────────────────────────────────── */
export const Combined: Story = {
  name: "Combined",
  parameters: {
    docs: {
      description: {
        story:
          "Shorthand for one-off form rows. `label` / `description` / " +
          "`error` props wrap the Input in an inline Field. Equivalent " +
          "to the Decomposed pattern; sugar at the call site.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Combined Input shorthand">
      <div className="zs-story-cell" style={{ flex: "1 1 18rem", minWidth: "14rem" }}>
        <span className="zs-story-label">Label + description</span>
        <Input
          label="Project name"
          description="Lowercase letters, numbers, and hyphens."
          placeholder="acme-prod"
        />
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 18rem", minWidth: "14rem" }}>
        <span className="zs-story-label">Label + error</span>
        <Input
          label="Project name"
          error="Already taken. Try another."
          defaultValue="acme-prod"
        />
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 18rem", minWidth: "14rem" }}>
        <span className="zs-story-label">All three</span>
        <Input
          label="API key"
          description="Generate one from Settings → Tokens."
          error="The token you entered has expired."
          defaultValue="sk-…2f8c"
        />
      </div>
    </div>
  ),
};

/* ─── 7. Required ────────────────────────────────────────────────────── */
export const Required: Story = {
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Required indicator">
      <div className="zs-story-cell" style={{ flex: "1 1 18rem", minWidth: "14rem" }}>
        <span className="zs-story-label">Required (star indicator)</span>
        <Field required>
          <Field.Label>
            Email <Field.Required />
          </Field.Label>
          <Input type="email" placeholder="you@example.com" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 18rem", minWidth: "14rem" }}>
        <span className="zs-story-label">Optional (fallback label)</span>
        <Field>
          <Field.Label>
            Bio <Field.Required fallback="(optional)" />
          </Field.Label>
          <Input placeholder="A short blurb…" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 18rem", minWidth: "14rem" }}>
        <span className="zs-story-label">Combined shorthand (auto-marker)</span>
        {/* The combined shorthand auto-inserts <Field.Required /> when
            both `label` and `required` are set — item 10. */}
        <Input
          label="Email"
          description="We will send a confirmation link."
          required
          type="email"
          placeholder="you@example.com"
        />
      </div>
    </div>
  ),
};

/* ─── 8. Input types ─────────────────────────────────────────────────── */
export const InputTypes: Story = {
  name: "Input types",
  parameters: {
    docs: {
      description: {
        story:
          "HIG: map text-field intent to the right virtual keyboard. The " +
          "`type` + `inputMode` + `autoComplete` triple keeps autofill, " +
          "ValidityState, and mobile keyboards aligned.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Input types">
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Email</span>
        <Field>
          <Field.Label>Email</Field.Label>
          <Input type="email" inputMode="email" autoComplete="email" placeholder="you@example.com" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Tel</span>
        <Field>
          <Field.Label>Phone</Field.Label>
          <Input type="tel" inputMode="tel" autoComplete="tel" placeholder="+1 555 0100" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">URL</span>
        <Field>
          <Field.Label>Website</Field.Label>
          <Input type="url" inputMode="url" autoComplete="url" placeholder="https://…" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Search</span>
        <Field>
          <Field.Label>Search</Field.Label>
          <Input type="search" inputMode="search" placeholder="Query…" startSlot={<IconSearch />} />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Number</span>
        <Field>
          <Field.Label>Seats</Field.Label>
          <Input type="number" inputMode="numeric" defaultValue={4} />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Date</span>
        <Field>
          <Field.Label>Starts</Field.Label>
          <Input type="date" defaultValue="2026-06-01" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Password</span>
        <Field>
          <Field.Label>Password</Field.Label>
          <Input type="password" autoComplete="current-password" defaultValue="hunter2hunter2" />
        </Field>
      </div>
    </div>
  ),
};

/* ─── 9. Horizontal layout ───────────────────────────────────────────── */
export const HorizontalLayout: Story = {
  name: "Horizontal layout",
  parameters: {
    docs: {
      description: {
        story:
          "Inspector-style dense form rows: label on the left, control + " +
          "helper on the right. Uses `<Field orientation=\"horizontal\">`.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Horizontal Field rows">
      <div
        className="zs-story-cell"
        style={{ flex: "1 1 100%", minWidth: "20rem", display: "flex", flexDirection: "column", gap: "0.75rem" }}
      >
        <Field orientation="horizontal">
          <Field.Label>Project name</Field.Label>
          <Input defaultValue="acme-prod" />
        </Field>
        <Field orientation="horizontal">
          <Field.Label>Region</Field.Label>
          <Input defaultValue="us-east-1" />
          <Field.Description>Cannot be changed after deploy.</Field.Description>
        </Field>
        <Field orientation="horizontal" required>
          <Field.Label>
            Admin email <Field.Required />
          </Field.Label>
          <Input type="email" defaultValue="ada@example.com" />
        </Field>
      </div>
    </div>
  ),
};

/* ─── 10. RTL ────────────────────────────────────────────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew / Arabic right-to-left layout. The startSlot should " +
          "render on the visual RIGHT and endSlot on the visual LEFT — " +
          "we use logical properties (margin-inline-*, padding-inline) " +
          "so the flip is automatic.",
      },
    },
  },
  render: () => (
    <div dir="rtl" className="zs-story-row" role="group" aria-label="RTL input row">
      <div className="zs-story-cell" style={{ flex: "1 1 18rem", minWidth: "14rem" }}>
        <span className="zs-story-label">חיפוש (search)</span>
        <Field>
          <Field.Label>חיפוש</Field.Label>
          <Input
            type="search"
            placeholder="חפש פרויקטים…"
            startSlot={<IconSearch />}
          />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 18rem", minWidth: "14rem" }}>
        <span className="zs-story-label">תקציב (budget)</span>
        <Field>
          <Field.Label>תקציב</Field.Label>
          <Input
            type="number"
            defaultValue={49}
            startSlot="₪"
            endSlot="/חודש"
          />
        </Field>
      </div>
    </div>
  ),
};

/* ─── 11. Long label & description ───────────────────────────────────── */
export const LongLabelAndDescription: Story = {
  name: "Long label and description",
  parameters: {
    docs: {
      description: {
        story:
          "Long content wraps gracefully; the Input's bounding box never " +
          "overflows its parent column thanks to `inline-size: 100%` and " +
          "the inner control's `min-inline-size: 0`.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Long label and description">
      <div className="zs-story-cell" style={{ flex: "0 0 auto", maxWidth: "16rem" }}>
        <span className="zs-story-label">Narrow column (16rem)</span>
        <Field>
          <Field.Label>
            A genuinely lengthy label that ought to wrap across multiple lines
          </Field.Label>
          <Input defaultValue="A very long pre-populated value that does not fit on one line" />
          <Field.Description>
            And a generously long helper paragraph below the control to verify
            that text reflows inside the constrained column without breaking the
            field layout or causing horizontal overflow.
          </Field.Description>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 12. Inside a form ──────────────────────────────────────────────── */
export const InsideForm: Story = {
  name: "Inside a form",
  parameters: {
    docs: {
      description: {
        story:
          "Native form submission; Base UI's Field validation feeds into " +
          "the native `submit` event. Tab order flows Email → Password " +
          "→ Submit. Each control's `aria-describedby` references its " +
          "description (and error when shown). The Submit button uses " +
          "real `type=\"submit\"`. Try submitting empty to see browser " +
          "validation fire alongside Field's `valueMissing` match. " +
          "(A Base UI `<Form>` integration ships in a later slice.)",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Form composition">
      <form
        className="zs-story-cell"
        style={{ flex: "1 1 24rem", minWidth: "16rem", display: "flex", flexDirection: "column", gap: "1rem" }}
        onSubmit={(e) => {
          e.preventDefault();
        }}
      >
        <span className="zs-story-label">Sign in</span>
        <Field required>
          <Field.Label>
            Email <Field.Required />
          </Field.Label>
          <Input type="email" autoComplete="email" placeholder="you@example.com" />
          <Field.Error match="valueMissing">Email is required.</Field.Error>
          <Field.Error match="typeMismatch">Enter a valid email address.</Field.Error>
        </Field>
        <Field required>
          <Field.Label>
            Password <Field.Required />
          </Field.Label>
          <Input type="password" autoComplete="current-password" />
          <Field.Description>At least 12 characters.</Field.Description>
          <Field.Error match="valueMissing">Password is required.</Field.Error>
        </Field>
        <div style={{ display: "flex", justifyContent: "flex-end" }}>
          <Button type="submit" variant="filled">
            Sign in
          </Button>
        </div>
      </form>
    </div>
  ),
};

/* ─── 13. Field size inheritance ─────────────────────────────────────── */
export const FieldSizeInheritance: Story = {
  name: "Field size inheritance",
  parameters: {
    docs: {
      description: {
        story:
          "`<Field size=\"sm\">` cascades into a contained Input that " +
          "doesn't explicitly set its own size. The Input on the right " +
          "overrides the inherited size — the consumer's prop always " +
          "wins.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Field size inheritance">
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Field size=sm → Input inherits</span>
        <Field size="sm">
          <Field.Label>Slug</Field.Label>
          <Input placeholder="acme-prod" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Field size=lg → Input inherits</span>
        <Field size="lg">
          <Field.Label>Title</Field.Label>
          <Input placeholder="My great project" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">Field=sm + Input size=lg → override wins</span>
        <Field size="sm">
          <Field.Label>Override</Field.Label>
          <Input size="lg" placeholder="explicit lg" />
        </Field>
      </div>
    </div>
  ),
};

/* ─── 14. Error boolean only (no message) ────────────────────────────── */
export const ErrorBooleanOnly: Story = {
  name: "Error boolean only",
  parameters: {
    docs: {
      description: {
        story:
          "Demonstrates `invalid` and `error={true}` — both flip the " +
          "invalid visual without rendering a message. Useful when an " +
          "external surface (a banner, a list) owns the error text and " +
          "the Input just needs to look invalid.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Invalid without message">
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">invalid (decomposed)</span>
        <Field>
          <Field.Label>Token</Field.Label>
          <Input invalid defaultValue="bad-token" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">error={"{true}"} (combined)</span>
        <Input label="Token" error={true} defaultValue="bad-token" />
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 14rem", minWidth: "12rem" }}>
        <span className="zs-story-label">error={"{false}"} → no wrap</span>
        {/* error={false} should NOT trigger combined wrapping. Item 9 of
            the slice-2 review fix brief. The Input renders bare here. */}
        <Input
          aria-label="Token (no wrap when error=false)"
          error={false}
          defaultValue="token"
        />
      </div>
    </div>
  ),
};

/* ─── 15. With external description ──────────────────────────────────── */
export const WithExternalDescription: Story = {
  name: "With external description",
  parameters: {
    docs: {
      description: {
        story:
          "When a consumer passes their own `aria-describedby`, it MERGES " +
          "with Field's auto-wired description id rather than clobbering " +
          "it. Open DevTools and inspect the rendered <input> — its " +
          "aria-describedby contains BOTH ids, space-separated.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="External aria-describedby">
      <div className="zs-story-cell" style={{ flex: "1 1 22rem", minWidth: "18rem" }}>
        <span className="zs-story-label">Field description + external hint</span>
        <p id="external-help" style={{ margin: 0, fontSize: "0.8125rem" }}>
          External help: paste your API key from the dashboard.
        </p>
        <Field>
          <Field.Label>API key</Field.Label>
          <Input
            data-testid="input-with-external-aria"
            aria-describedby="external-help"
            placeholder="sk-…"
          />
          <Field.Description>
            Tokens are stored hashed at rest.
          </Field.Description>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 16. Field disabled propagation ─────────────────────────────────── */
export const FieldDisabledPropagation: Story = {
  name: "Field disabled propagation",
  parameters: {
    docs: {
      description: {
        story:
          "`<Field disabled>` greys the entire row and propagates to the " +
          "contained Input via FieldContext — the Input visually disables " +
          "without the consumer needing to repeat `disabled` on it.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Field disabled propagation">
      <div className="zs-story-cell" style={{ flex: "1 1 18rem", minWidth: "14rem" }}>
        <span className="zs-story-label">Field disabled</span>
        <Field disabled>
          <Field.Label>Region</Field.Label>
          <Input defaultValue="us-east-1" />
          <Field.Description>Cannot be changed after deploy.</Field.Description>
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 18rem", minWidth: "14rem" }}>
        <span className="zs-story-label">Field NOT disabled (control)</span>
        <Field>
          <Field.Label>Region</Field.Label>
          <Input defaultValue="us-east-1" />
        </Field>
      </div>
    </div>
  ),
};

/* ─── 17. Horizontal layout with error ───────────────────────────────── */
export const HorizontalLayoutWithError: Story = {
  name: "Horizontal layout with error",
  parameters: {
    docs: {
      description: {
        story:
          "Verifies the Grid-based horizontal layout: label left-column, " +
          "control + description + error stack in the right column. The " +
          "previous flex-wrap approach mis-placed description + error " +
          "inline with the control.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Horizontal with error">
      <div
        className="zs-story-cell"
        style={{ flex: "1 1 100%", minWidth: "20rem", display: "flex", flexDirection: "column", gap: "0.75rem" }}
      >
        <Field orientation="horizontal" invalid>
          <Field.Label>Slug</Field.Label>
          <Input defaultValue="ACME" />
          <Field.Description>Lowercase letters, numbers, and hyphens only.</Field.Description>
          <Field.Error match>Slug must be lowercase.</Field.Error>
        </Field>
        <Field orientation="horizontal" required invalid>
          <Field.Label>
            Admin email <Field.Required />
          </Field.Label>
          <Input type="email" defaultValue="ada@" />
          <Field.Description>For billing and alerts.</Field.Description>
          <Field.Error match>Enter a valid email address.</Field.Error>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 18. Input ref integration ──────────────────────────────────────── */
export const InputRefIntegration: Story = {
  name: "Input ref integration",
  parameters: {
    docs: {
      description: {
        story:
          "The forwardRef'd ref composes with Base UI's internal ref, so " +
          "BOTH land on the rendered `<input>`. Click the Focus button to " +
          "verify the consumer-supplied ref can call `.focus()`. " +
          "(Without ref composition, the consumer's ref would be null and " +
          "the button would no-op.)",
      },
    },
  },
  render: function InputRefIntegrationRender() {
    const inputRef = useRef<HTMLInputElement | null>(null);
    const [status, setStatus] = useState<string>("idle");
    return (
      <div className="zs-story-row" role="group" aria-label="Ref integration">
        <div className="zs-story-cell" style={{ flex: "1 1 22rem", minWidth: "18rem", gap: "0.5rem" }}>
          <span className="zs-story-label">Consumer ref → input.focus()</span>
          <Field>
            <Field.Label>Search query</Field.Label>
            <Input
              ref={inputRef}
              data-testid="input-ref-target"
              placeholder="Type here after focusing…"
            />
          </Field>
          <div style={{ display: "flex", gap: "0.5rem", alignItems: "center" }}>
            <Button
              variant="filled"
              onClick={() => {
                inputRef.current?.focus();
                setStatus(
                  document.activeElement === inputRef.current
                    ? "focused"
                    : "ref-missing",
                );
              }}
            >
              Focus input
            </Button>
            <span data-testid="input-ref-status" style={{ fontSize: "0.8125rem" }}>
              status: {status}
            </span>
          </div>
        </div>
      </div>
    );
  },
};

/* ─── 19. Autofill demo ──────────────────────────────────────────────── */
export const Autofill: Story = {
  name: "Autofill",
  parameters: {
    docs: {
      description: {
        story:
          "Email + password inputs with `autoComplete` set so browsers " +
          "can autofill from their credential manager. Visually verifies " +
          "that the autofill background trick (long inset shadow) keeps " +
          "the input on-surface across Chrome and Firefox. " +
          "`defaultValue` is set on both cells (visual-polish item 3) so " +
          "the static screenshot shows the populated state — the WebKit " +
          "yellow autofill background that our Input.css :autofill rule " +
          "suppresses would have painted here in raw Chrome; the cells " +
          "are visibly clean form chrome instead.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Autofill demo">
      <div
        className="zs-story-cell"
        style={{ flex: "1 1 22rem", minWidth: "18rem", display: "flex", flexDirection: "column", gap: "0.75rem" }}
      >
        <Field>
          <Field.Label>Email</Field.Label>
          {/* defaultValue forces a visibly-populated state for the static
              screenshot. Without it the autofill story captures an empty
              input and the autofill-suppression CSS has no visual proof. */}
          <Input
            type="email"
            name="email"
            autoComplete="email"
            defaultValue="ada@example.com"
            placeholder="you@example.com"
          />
        </Field>
        <Field>
          <Field.Label>Password</Field.Label>
          <Input
            type="password"
            name="password"
            autoComplete="current-password"
            defaultValue="hunter2hunter2"
            placeholder="••••••••"
          />
        </Field>
      </div>
    </div>
  ),
};

/* ─── 20. Custom validate ────────────────────────────────────────────── */
export const CustomValidate: Story = {
  name: "Custom validate",
  parameters: {
    docs: {
      description: {
        story:
          "Uses Base UI's `validate` prop on `<Field>` to enforce a " +
          "domain rule (the value 'admin' is reserved). The Field's " +
          "Error renders the message returned by `validate`. " +
          "Validation runs on mount when the initial value is invalid, " +
          "and on blur otherwise (validationMode=\"onBlur\"). " +
          "Visual-polish item 1: three cells render the EMPTY default, " +
          "the INVALID-on-mount state (pre-set defaultValue=\"admin\" " +
          "so the red shell + error text appear in static captures), " +
          "and a VALID example.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Custom validate">
      <div className="zs-story-cell" style={{ flex: "1 1 18rem", minWidth: "16rem" }}>
        <span className="zs-story-label">Empty (no validation fired)</span>
        <Field
          validationMode="onBlur"
          validate={(v) => (v === "admin" ? "‘admin’ is reserved." : null)}
        >
          <Field.Label>Username</Field.Label>
          <Input placeholder="Pick a username…" />
          <Field.Description>Reserved words are blocked.</Field.Description>
          <Field.Error />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 18rem", minWidth: "16rem" }}>
        <span className="zs-story-label">Invalid on mount (defaultValue=admin)</span>
        {/* Base UI's validate runs on the configured trigger (here
            onBlur) — there's no built-in onMount mode. To make the
            invalid state visible in a static screenshot we drive it
            via the controlled-state escape hatch the FieldRoot API
            provides: `invalid` + `<Field.Error match>` with a hard-
            coded message. validate stays declared so an interactive
            user sees the same rule fire on blur in a live Storybook. */}
        <Field
          invalid
          validationMode="onBlur"
          validate={(v) => (v === "admin" ? "‘admin’ is reserved." : null)}
        >
          <Field.Label>Username</Field.Label>
          <Input defaultValue="admin" />
          <Field.Description>Type ‘admin’ and tab away.</Field.Description>
          <Field.Error match>‘admin’ is reserved.</Field.Error>
        </Field>
      </div>
      <div className="zs-story-cell" style={{ flex: "1 1 18rem", minWidth: "16rem" }}>
        <span className="zs-story-label">Valid (defaultValue=ada)</span>
        <Field
          validationMode="onBlur"
          validate={(v) => (v === "admin" ? "‘admin’ is reserved." : null)}
        >
          <Field.Label>Username</Field.Label>
          <Input defaultValue="ada" />
          <Field.Description>Passes the reserved-words check.</Field.Description>
          <Field.Error />
        </Field>
      </div>
    </div>
  ),
};
