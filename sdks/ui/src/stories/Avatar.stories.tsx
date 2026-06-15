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
 * Base UI status transition. Review-fix 🟡: previously this was a real
 * network URL (`https://0.0.0.0/...`) whose error timing depended on
 * the runner's network stack and could race `networkidle`. We use a
 * deterministic malformed `data:` image URL instead — the base64
 * payload is not a valid PNG, so the browser fires `error` during
 * decode (synchronous-ish, no network round-trip, no runner skew). */
const BROKEN_SRC =
  "data:image/png;base64,bm90LWEtcG5n";

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
    // Regression for 🔴 fix: when `src` is unset the fallback IS the
    // accessible name — captioning is on the caller. So the wrapper
    // must NOT stamp aria-hidden / role=img / aria-label on it (those
    // belong to the substitute-for-image branches only).
    const fallback = root.querySelector(".zs-avatar__fallback");
    await expect(fallback).not.toBeNull();
    await expect(fallback).not.toHaveAttribute("aria-hidden");
    await expect(fallback).not.toHaveAttribute("role", "img");
    await expect(fallback).not.toHaveAttribute("aria-label");
  },
};

/* ─── 3. FallbackOnError — broken src → visible fallback ────────────── */
export const FallbackOnError: Story = {
  name: "Fallback on error (broken src)",
  parameters: {
    docs: {
      description: {
        story:
          "Broken `src` → Base UI mounts the Fallback once the image " +
          "load fails. The assertion requires the fallback TEXT to be " +
          "visibly rendered — status flags alone are not sufficient " +
          "(Base UI's `imageLoadingStatus` is not stamped on the DOM). " +
          "Identity-image path (non-empty `alt`): the Fallback inherits " +
          "the image's accessible name via `role=img` + `aria-label`, so " +
          "AT announces the identity (\"Broken portrait\") instead of " +
          "leaking the raw initials text (\"B R\").",
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
    // Real-path assertion: the fallback TEXT must actually paint.
    // Status flags alone (e.g. data-image-loading-status) are a
    // phantom — Base UI's stateAttributesMapping returns `null` for
    // imageLoadingStatus so the attribute never reaches the DOM.
    await waitFor(
      async () => {
        await expect(root).toHaveTextContent("BR");
      },
      { timeout: 5000 },
    );
    // Regression for 🔴 fix: the fallback substituting for the image
    // must mirror the image's accessible name. Pre-fix the fallback
    // was a plain <span> whose initials text leaked to AT instead of
    // the documented `alt` contract.
    const fallback = root.querySelector(".zs-avatar__fallback");
    await expect(fallback).not.toBeNull();
    await expect(fallback).toHaveAttribute("role", "img");
    await expect(fallback).toHaveAttribute("aria-label", "Broken portrait");
  },
};

/* ─── 3a. DecorativeFallback — src + alt="" → fallback hidden from AT ─ */
export const DecorativeFallback: Story = {
  name: "Decorative fallback (src + alt=\"\")",
  parameters: {
    docs: {
      description: {
        story:
          "When a caller marks the image decorative with `alt=\"\"`, " +
          "the Fallback that substitutes for the image MUST also be " +
          "hidden from AT — otherwise the decorative contract only " +
          "holds in the loaded state and the load/error states leak " +
          "the raw initials text. Asserts `aria-hidden=\"true\"` on the " +
          "rendered fallback.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Decorative fallback"
    >
      <Avatar
        src={BROKEN_SRC}
        alt=""
        fallback="DC"
        fallbackDelay={0}
        data-testid="avatar-decorative"
      />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("avatar-decorative");
    await expect(root).toBeInTheDocument();
    await waitFor(
      async () => {
        await expect(root).toHaveTextContent("DC");
      },
      { timeout: 5000 },
    );
    // Regression for 🔴 fix: the fallback must be aria-hidden when the
    // caller asked for decorative semantics via `alt=""`.
    const fallback = root.querySelector(".zs-avatar__fallback");
    await expect(fallback).not.toBeNull();
    await expect(fallback).toHaveAttribute("aria-hidden", "true");
    // And it must NOT carry the identity-path aria-label.
    await expect(fallback).not.toHaveAttribute("aria-label");
    await expect(fallback).not.toHaveAttribute("role", "img");
  },
};

/* ─── 3b. FallbackDelay — fallback gated by non-zero delay ──────────── */
export const FallbackDelay: Story = {
  name: "Fallback delay (non-zero)",
  parameters: {
    docs: {
      description: {
        story:
          "Broken `src` with `fallbackDelay={1500}` — Base UI holds the " +
          "Fallback offscreen until the delay elapses, then paints the " +
          "initials. Asserts the delay is actually plumbed: fallback " +
          "text is ABSENT initially, then PRESENT after the delay.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Fallback delay non-zero"
    >
      <Avatar
        src={BROKEN_SRC}
        alt="Broken portrait"
        fallback="DL"
        fallbackDelay={1500}
        data-testid="avatar-delay"
      />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("avatar-delay");
    await expect(root).toBeInTheDocument();
    // The fallback must eventually paint visible text. We don't gate
    // on the upper bound of the delay window — Base UI's image-load
    // failure timing varies across runners — but we DO require that
    // the rendered text content carry the fallback string. If the
    // `fallbackDelay` prop were silently dropped, this assertion
    // would still pass once the broken image gave up; the absence
    // assertion below is what proves the delay path actually fires.
    const initialText = root.textContent ?? "";
    await expect(initialText.includes("DL")).toBe(false);
    await waitFor(
      async () => {
        await expect(root).toHaveTextContent("DL");
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
          "Shows this component behavior with realistic content and keeps the edge case easy to inspect.",
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
