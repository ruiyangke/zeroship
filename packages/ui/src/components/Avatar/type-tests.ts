/*
 * Type-only regression tests for Avatar's discriminated union (review
 * 🟡 fix). Pre-fix `src?: string; alt?: string` left a path where
 * `<Avatar src="…" />` typechecked and silently ran an `<img>` whose
 * accessible name was the raw URL. The union below verifies the new
 * shape: passing `src` REQUIRES `alt`; the fallback-only branch
 * rejects `alt`.
 *
 * Compiled by `tsc --noEmit` via the package's typecheck — TypeScript
 * errors are surfaced by `@ts-expect-error` lines whose absence (i.e.
 * the next-line check stops erroring) would fail the build. Same
 * convention Toggle.Group / Select / Field type-tests use.
 */
import { createElement } from "react";
import { Avatar } from "./Avatar";

// ─── Item 1: src + alt compiles ───────────────────────────────────────
function _srcAndAltCompiles() {
  return createElement(Avatar, {
    src: "https://example.com/ada.png",
    alt: "Ada Lovelace",
    fallback: "AL",
  });
}

// Decorative path: src + alt="" compiles.
function _srcAndEmptyAltCompiles() {
  return createElement(Avatar, {
    src: "https://example.com/ada.png",
    alt: "",
    fallback: "AL",
  });
}

// ─── Item 2: src without alt is rejected at compile time ──────────────
function _srcWithoutAltRejected() {
  return createElement(
    Avatar,
    // @ts-expect-error — `src` requires `alt` in the same prop bag.
    { src: "https://example.com/ada.png", fallback: "AL" },
  );
}

// ─── Item 3: fallback-only branch compiles ────────────────────────────
function _fallbackOnlyCompiles() {
  return createElement(Avatar, { fallback: "AL" });
}

// ─── Item 4: fallback-only branch rejects stray `alt` ─────────────────
// (Without `src` there is no image to caption; passing `alt` here is a
// bug. The discriminated `AvatarPropsWithoutSrc` declares `alt?: undefined`,
// so a string `alt` is a type error.)
function _fallbackOnlyRejectsAlt() {
  return createElement(
    Avatar,
    // @ts-expect-error — fallback-only branch must not carry an `alt`.
    { fallback: "AL", alt: "Ada" },
  );
}

// ─── Item 5: Avatar.Image requires `alt` ──────────────────────────────
function _avatarImageRequiresAlt() {
  return createElement(
    Avatar.Image,
    // @ts-expect-error — Avatar.Image requires `alt` (empty string OK,
    // missing string not OK).
    { src: "https://example.com/ada.png" },
  );
}

function _avatarImageWithAltCompiles() {
  return createElement(Avatar.Image, {
    src: "https://example.com/ada.png",
    alt: "Ada Lovelace",
  });
}

function _avatarImageWithEmptyAltCompiles() {
  return createElement(Avatar.Image, {
    src: "https://example.com/ada.png",
    alt: "",
  });
}
