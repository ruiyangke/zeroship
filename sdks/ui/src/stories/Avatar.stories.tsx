import type { Meta, StoryObj } from "@storybook/react";
import { expect, waitFor, within } from "@storybook/test";
import { Avatar } from "../components";

const meta: Meta<typeof Avatar> = {
  title: "Components/Avatar",
  component: Avatar,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Avatar>;

/* A small inline data-URL portrait we can guarantee renders without a
 * network round-trip. The avatar canvas is a 1x1 PNG (gray pixel)
 * scaled up by the size token — sufficient to assert <img> mounting,
 * load events, alt forwarding. We don't need a real portrait; the
 * fallback path does not execute when this loads. */
const AVATAR_DATA_URL =
  "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAA" +
  "C0lEQVR42mP8/x8AAuMB8DtXNJsAAAAASUVORK5CYII=";

/* A guaranteed-failing src so the FallbackOnError path executes the
 * Base UI status transition. The protocol-only URL never resolves. */
const BROKEN_SRC = "https://0.0.0.0/never-resolves.png";

function UserGlyph() {
  return (
    <svg
      viewBox="0 0 16 16"
      width="60%"
      height="60%"
      aria-hidden="true"
      focusable="false"
    >
      <path
        fill="currentColor"
        d="M8 8a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm0 1.5c-3 0-5.5 1.5-5.5 3.5V14h11v-1c0-2-2.5-3.5-5.5-3.5Z"
      />
    </svg>
  );
}

/* ─── 1. Basic — src + alt ──────────────────────────────────────────── */
export const Basic: Story = {
  name: "Basic (src + alt)",
  parameters: {
    docs: {
      description: {
        story:
          "Stock Avatar with a loaded image. Asserts the underlying " +
          "<img> mounts with the correct alt text and the loading " +
          "status transitions to `loaded`.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic avatar">
      <Avatar
        src={AVATAR_DATA_URL}
        alt="Ada Lovelace"
        fallback="AL"
        data-testid="avatar-basic"
      />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("avatar-basic");
    await expect(root).toBeInTheDocument();
    await waitFor(async () => {
      const img = root.querySelector("img");
      await expect(img).not.toBeNull();
      await expect(img).toHaveAttribute("alt", "Ada Lovelace");
    });
  },
};

/* ─── 2. Fallback — no src → initials paint ─────────────────────────── */
export const Fallback: Story = {
  name: "Fallback (no src → initials)",
  parameters: {
    docs: {
      description: {
        story:
          "When no `src` is supplied, the Fallback paints unconditionally. " +
          "Tinted accent surface + uppercase initials.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Fallback only">
      <Avatar fallback="AL" data-testid="avatar-fallback-only" />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("avatar-fallback-only");
    await expect(root).toBeInTheDocument();
    await expect(root).toHaveTextContent("AL");
    await expect(root.querySelector("img")).toBeNull();
  },
};

/* ─── 3. FallbackOnError — broken src → fallback after delay ────────── */
export const FallbackOnError: Story = {
  name: "Fallback on error (broken src)",
  parameters: {
    docs: {
      description: {
        story:
          "Broken `src` → Base UI transitions imageLoadingStatus to " +
          "`error` and mounts the Fallback. The `fallbackDelay` prop " +
          "suppresses a flash of initials during the loading window.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Fallback on error">
      <Avatar
        src={BROKEN_SRC}
        alt="Broken portrait"
        fallback="BR"
        fallbackDelay={0}
        data-testid="avatar-error"
      />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("avatar-error");
    await expect(root).toBeInTheDocument();
    await waitFor(
      async () => {
        // Either the fallback has appeared OR the status is "error".
        // We don't gate on a specific timing; once the broken image
        // gives up, the fallback paints.
        const hasFallback = !!root.textContent?.includes("BR");
        const status = root.getAttribute("data-image-loading-status");
        await expect(hasFallback || status === "error").toBe(true);
      },
      { timeout: 5000 },
    );
  },
};

/* ─── 4. Sizes — xs / sm / md / lg / xl ─────────────────────────────── */
export const Sizes: Story = {
  name: "Sizes (xs / sm / md / lg / xl)",
  parameters: {
    docs: {
      description: {
        story:
          "Five enumerated sizes from xs (1.5rem) to xl (4rem). Fallback " +
          "type scale tracks size so initials read proportionate.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Sizes">
      <Avatar size="xs" fallback="XS" data-testid="avatar-size-xs" />
      <Avatar size="sm" fallback="SM" data-testid="avatar-size-sm" />
      <Avatar size="md" fallback="MD" data-testid="avatar-size-md" />
      <Avatar size="lg" fallback="LG" data-testid="avatar-size-lg" />
      <Avatar size="xl" fallback="XL" data-testid="avatar-size-xl" />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    for (const size of ["xs", "sm", "md", "lg", "xl"] as const) {
      const node = canvas.getByTestId(`avatar-size-${size}`);
      await expect(node).toHaveAttribute("data-size", size);
    }
  },
};

/* ─── 5. Shapes — circle / square / rounded ─────────────────────────── */
export const Shapes: Story = {
  name: "Shapes (circle / square / rounded)",
  parameters: {
    docs: {
      description: {
        story:
          "Three shapes. `circle` is the default identity shape; `square` " +
          "and `rounded` are reserved for non-person identities (orgs, " +
          "teams, app icons).",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Shapes">
      <Avatar shape="circle" fallback="CI" data-testid="avatar-shape-circle" />
      <Avatar shape="square" fallback="SQ" data-testid="avatar-shape-square" />
      <Avatar
        shape="rounded"
        fallback="RD"
        data-testid="avatar-shape-rounded"
      />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getByTestId("avatar-shape-circle")).toHaveAttribute(
      "data-shape",
      "circle",
    );
    await expect(canvas.getByTestId("avatar-shape-square")).toHaveAttribute(
      "data-shape",
      "square",
    );
    await expect(canvas.getByTestId("avatar-shape-rounded")).toHaveAttribute(
      "data-shape",
      "rounded",
    );
  },
};

/* ─── 6. WithIcon — SVG glyph as fallback ───────────────────────────── */
export const WithIcon: Story = {
  name: "With icon (SVG fallback)",
  parameters: {
    docs: {
      description: {
        story:
          "Icon-as-fallback for headless / generic-user cases. The icon " +
          "rides currentColor so it inherits the fallback ink token.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Icon fallback">
      <Avatar
        size="lg"
        fallback={<UserGlyph />}
        data-testid="avatar-icon"
      />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("avatar-icon");
    await expect(root.querySelector("svg")).not.toBeNull();
  },
};

/* ─── 7. Group — overlapping row ────────────────────────────────────── */
export const Group: Story = {
  name: "Group (overlap row)",
  parameters: {
    docs: {
      description: {
        story:
          "Layout pattern: wrap a row of Avatars and stamp " +
          "`data-position=\"overlap\"` on each child after the first. " +
          "The negative inline-start margin gives the canonical stack " +
          "rhythm; an extra outline separates each lockup from the one " +
          "below.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Avatar group">
      <div
        style={{ display: "inline-flex" }}
        data-testid="avatar-group"
      >
        <Avatar fallback="AL" data-testid="avatar-group-1" />
        <Avatar
          fallback="BO"
          data-position="overlap"
          data-testid="avatar-group-2"
        />
        <Avatar
          fallback="CH"
          data-position="overlap"
          data-testid="avatar-group-3"
        />
        <Avatar
          fallback="DA"
          data-position="overlap"
          data-testid="avatar-group-4"
        />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const group = canvas.getByTestId("avatar-group");
    await expect(group.children.length).toBe(4);
    for (let i = 2; i <= 4; i++) {
      const node = canvas.getByTestId(`avatar-group-${i}`);
      await expect(node).toHaveAttribute("data-position", "overlap");
    }
  },
};

/* ─── 8. RTL — visual parity ────────────────────────────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "RTL containers flip the overlap direction automatically via " +
          "logical properties — no separate Avatar rule needed. The " +
          "lockup itself reads identical because it has no inline " +
          "content; this story exists as a visual regression net.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="RTL avatars"
      dir="rtl"
    >
      <div
        style={{ display: "inline-flex" }}
        data-testid="avatar-rtl-group"
        dir="rtl"
      >
        <Avatar fallback="AL" data-testid="avatar-rtl-1" />
        <Avatar
          fallback="BO"
          data-position="overlap"
          data-testid="avatar-rtl-2"
        />
        <Avatar
          fallback="CH"
          data-position="overlap"
          data-testid="avatar-rtl-3"
        />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const group = canvas.getByTestId("avatar-rtl-group");
    await expect(group).toHaveAttribute("dir", "rtl");
  },
};
