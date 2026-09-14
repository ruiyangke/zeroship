/*
 * Avatar — identity lockup (image with fallback).
 *
 * Wraps Base UI's `Avatar` namespace (`Avatar.Root`, `Avatar.Image`,
 * `Avatar.Fallback`). The composed surface is content-shape: a tinted
 * square / circle that swaps to a real `<img>` when one is supplied
 * and falls back to initials / icon glyphs while the image is loading
 * or after a load failure.
 *
 *   <Avatar src="…" alt="Ada Lovelace" fallback="AL" />
 *
 * Or, decomposed for advanced layouts:
 *
 *   <Avatar.Root size="lg">
 *     <Avatar.Image src="…" alt="Ada" />
 *     <Avatar.Fallback delay={400}>AL</Avatar.Fallback>
 *   </Avatar.Root>
 *
 * Design guarantees encoded here (in source so they travel with the
 * code, not in a sibling doc):
 *
 *   1. Identity-conveying shape. The default is `circle` — that's the
 *      shape the eye reads as "person". `square` and `rounded` exist
 *      for org / team / app icons where a circle would over-claim
 *      personhood. Sizing is enumerated (xs / sm / md / lg / xl) so
 *      every rendered row reads coherent against neighbouring controls.
 *
 *   2. Fallback first, image second. Base UI mounts Fallback while the
 *      image loads and after errors. We render Fallback unconditionally
 *      when `src` is omitted and conditionally (via Base UI's status)
 *      when `src` is present. The `fallbackDelay` prop (ms) suppresses
 *      a flash of initials on fast loads; the default of `0` means the
 *      fallback paints immediately and the image takes over once it
 *      decodes.
 *
 *   3. `alt` is required when `src` is set; a decorative image should
 *      pass `alt=""` explicitly (Base UI then keeps the empty string
 *      and AT skips the node). The dev-warn fires once per signature.
 *
 *   4. Fallback initials inherit the parent's color via `--zs-label`
 *      and tint over a translucent accent surface so the avatar reads
 *      as identity-tinted glass against any background — not as a
 *      flat solid swatch that fights the canvas underneath.
 *
 *   5. Group overlap is a layout concern, not a component flag. Wrap
 *      a row of Avatars in any container and set `data-position` on
 *      the children to overlap them (the Group story exercises the
 *      pattern); no Avatar.Group component required.
 *
 *   6. forced-colors specificity mirror is at the same selector
 *      depth as the default state so the system palette wins. RTL is
 *      a no-op (Avatar carries no inline-direction layout); we still
 *      verify the visual stays identical via an RTL story.
 *
 *   7. Reduced motion is a no-op here — Avatar has no transitions. The
 *      gate exists for parity with the rest of the surface.
 *
 *   8. NOT a form control. Avatar is purely presentational; it carries
 *      no interactive role unless wrapped in a button / link.
 */
import {
  forwardRef,
  useEffect,
  type ComponentPropsWithRef,
  type ReactNode,
  type Ref,
} from "react";
import { Avatar as BaseAvatar } from "@base-ui/react/avatar";
import { classnames } from "../_classnames";

export type AvatarSize = "xs" | "sm" | "md" | "lg" | "xl";
export type AvatarShape = "circle" | "square" | "rounded";

type BaseRootProps = ComponentPropsWithRef<typeof BaseAvatar.Root>;
type BaseImageProps = ComponentPropsWithRef<typeof BaseAvatar.Image>;
type BaseFallbackProps = ComponentPropsWithRef<typeof BaseAvatar.Fallback>;

/* ─── dev-warn dedup ────────────────────────────────────────────────── *
 *
 * Avatar's only dev-warn today: a `src` set without a corresponding
 * `alt`. We dedupe by the `src` string so a busy gallery doesn't
 * drown in repeats. DCE'd in production via the static NODE_ENV
 * compare (same pattern Toggle.Group uses).
 */
const altWarned = new Set<string>();

/* ─── Avatar.Root ───────────────────────────────────────────────────── */

export interface AvatarRootProps
  extends Omit<BaseRootProps, "className" | "render"> {
  /**
   * Visual size — `xs` 1.5rem / `sm` 2rem / `md` 2.5rem (default) /
   * `lg` 3rem / `xl` 4rem. Drives both the container box AND the
   * fallback type scale so initials read proportionate to the lockup.
   */
  size?: AvatarSize;
  /**
   * Visual shape — `circle` (default) reads as "person"; `square` and
   * `rounded` are reserved for non-person identities (org logos, team
   * marks, app icons).
   */
  shape?: AvatarShape;
  /** Optional class hook on the root. */
  className?: string;
  /** Composed children (typically Avatar.Image + Avatar.Fallback). */
  children?: ReactNode;
}

const AvatarRoot = forwardRef<HTMLSpanElement, AvatarRootProps>(
  function AvatarRoot(
    { size = "md", shape = "circle", className, children, ...rest },
    ref,
  ) {
    return (
      <BaseAvatar.Root
        {...(rest as BaseRootProps)}
        ref={ref as Ref<HTMLSpanElement>}
        className={classnames(
          "zs-avatar",
          `zs-avatar--${size}`,
          `zs-avatar--${shape}`,
          className,
        )}
        data-size={size}
        data-shape={shape}
      >
        {children}
      </BaseAvatar.Root>
    );
  },
);
AvatarRoot.displayName = "Avatar.Root";

/* ─── Avatar.Image ──────────────────────────────────────────────────── *
 *
 * `alt` is REQUIRED here for the same reason it's required on the
 * shorthand: an `<img>` rendered into the lockup without `alt` either
 * leaks the raw URL to assistive tech (older AT defaulting to filename)
 * or fails WCAG 1.1.1 outright. Pass `alt=""` explicitly for decorative
 * portraits; pass a descriptive string for identity images. We can't
 * default to `""` because that would silently mark every identity
 * avatar decorative and ship a worse-than-nothing default. */

export interface AvatarImageProps
  extends Omit<BaseImageProps, "className" | "render" | "alt"> {
  /**
   * Optional class hook on the underlying `<img>`. The default class
   * paints the image flush against the Root's content-box at the
   * Root's `border-radius`.
   */
  className?: string;
  /**
   * Alt text for the image. REQUIRED. Pass `alt=""` for decorative
   * images (AT skips the node); pass a descriptive string for identity
   * images.
   */
  alt: string;
}

const AvatarImage = forwardRef<HTMLImageElement, AvatarImageProps>(
  function AvatarImage({ className, alt, ...rest }, ref) {
    return (
      <BaseAvatar.Image
        {...(rest as BaseImageProps)}
        ref={ref}
        alt={alt}
        className={classnames("zs-avatar__image", className)}
      />
    );
  },
);
AvatarImage.displayName = "Avatar.Image";

/* ─── Avatar.Fallback ───────────────────────────────────────────────── */

export interface AvatarFallbackProps
  extends Omit<BaseFallbackProps, "className" | "render"> {
  /**
   * Delay in ms before the fallback paints. Useful for suppressing a
   * flash of initials when the image is likely to arrive within a
   * few frames. Defaults to `0` (paint immediately; the image swaps
   * in once it decodes).
   */
  delay?: number;
  /** Optional class hook on the underlying `<span>`. */
  className?: string;
  /** Fallback contents — initials, an icon, or any ReactNode. */
  children?: ReactNode;
}

const AvatarFallback = forwardRef<HTMLSpanElement, AvatarFallbackProps>(
  function AvatarFallback({ className, children, ...rest }, ref) {
    return (
      <BaseAvatar.Fallback
        {...(rest as BaseFallbackProps)}
        ref={ref as Ref<HTMLSpanElement>}
        className={classnames("zs-avatar__fallback", className)}
      >
        {children}
      </BaseAvatar.Fallback>
    );
  },
);
AvatarFallback.displayName = "Avatar.Fallback";

/* ─── Shorthand Avatar ──────────────────────────────────────────────── *
 *
 * The 80% case: `<Avatar src="…" alt="…" fallback="AL" />`. Expands to
 * Root + Image + Fallback under the hood. Consumers needing finer
 * control (e.g. lazy-loading, IntersectionObserver) reach for the
 * decomposed surface via `Avatar.Root` / `Avatar.Image` / `Avatar.Fallback`.
 *
 * Discriminated union (review-fix 🟡): the public type contract
 * requires `alt` whenever `src` is set. Pre-fix the optional `alt?`
 * left a path where `<Avatar src="…" />` typechecked and then ran
 * through `Avatar.Image` with `alt={undefined}` — silently producing
 * an `<img>` whose accessible name is the raw URL. The union mirrors
 * Toggle.Group / Select's discriminated-union pattern: TypeScript
 * narrows by the presence of `src` and the missing `alt` becomes a
 * compile-time error.
 */
interface AvatarPropsBase extends Omit<AvatarRootProps, "children"> {
  /**
   * Fallback content — initials (1–2 uppercase chars), an icon, or
   * any ReactNode. Painted while the image loads and after errors;
   * painted unconditionally when `src` is omitted.
   */
  fallback?: ReactNode;
  /** Forwarded to Avatar.Fallback's `delay`. See AvatarFallbackProps. */
  fallbackDelay?: number;
  /**
   * Advanced composition slot — when set, replaces the auto-expanded
   * Image + Fallback children. Lets a consumer mix in extra nodes
   * (e.g. a status dot wrapper) without losing the Root's styling.
   */
  children?: ReactNode;
}

/** Image-bearing branch — `src` is set, so `alt` is required. */
export interface AvatarPropsWithSrc extends AvatarPropsBase {
  /** Image source — mounts Avatar.Image inside the lockup. */
  src: string;
  /**
   * Alt text for the image. REQUIRED whenever `src` is set. Pass
   * `alt=""` for decorative portraits (AT will skip the image AND the
   * fallback that substitutes for it); pass a descriptive string for
   * identity images (the fallback inherits that accessible name while
   * it stands in for the loading / broken image).
   */
  alt: string;
}

/** Fallback-only branch — no `src`, no `alt`. */
export interface AvatarPropsWithoutSrc extends AvatarPropsBase {
  src?: undefined;
  alt?: undefined;
}

export type AvatarProps = AvatarPropsWithSrc | AvatarPropsWithoutSrc;

const AvatarShorthand = forwardRef<HTMLSpanElement, AvatarProps>(
  function Avatar(props, ref) {
    const { src, alt, fallback, fallbackDelay, children, ...rootRest } =
      props as AvatarPropsBase & { src?: string; alt?: string };
    useEffect(() => {
      if (typeof process === "undefined") return;
      if (process.env.NODE_ENV === "production") return;
      if (src && alt === undefined) {
        const sig = `alt-missing:${src}`;
        if (!altWarned.has(sig)) {
          altWarned.add(sig);
          // eslint-disable-next-line no-console
          console.warn(
            `[Avatar] Rendered with src="${src}" but no \`alt\`. ` +
              "Pass `alt=\"\"` for decorative images, or a descriptive " +
              "string for identity images. AT users will hear the " +
              "raw URL otherwise.",
          );
        }
      }
    }, [src, alt]);

    /* ─── aria-wiring for the Fallback substitute (review-fix 🔴) ───
     *
     * When `src` is set the Fallback paints WHILE the image loads and
     * AFTER it errors — i.e. it stands in for the image. The image's
     * `alt` is the documented contract for the lockup's accessible
     * name; the fallback must mirror that contract, not leak a second
     * announcement of the raw initials text.
     *
     * - `alt === ""` (decorative): hide the fallback from AT so the
     *   decorative semantics the caller asked for hold across the
     *   load / error states, not only the loaded state.
     * - `alt` is a non-empty string (identity image): give the fallback
     *   `role="img"` + `aria-label={alt}` so AT announces the actual
     *   identity ("Ada Lovelace") instead of reading the raw initials
     *   ("A L") as text content.
     * - `src` unset (fallback-only avatar): leave the fallback as plain
     *   text so the initials ARE the accessible name. Captioning is
     *   the caller's responsibility in that case.
     */
    const fallbackA11y: {
      "aria-hidden"?: "true";
      role?: "img";
      "aria-label"?: string;
    } =
      src === undefined
        ? {}
        : alt === ""
          ? { "aria-hidden": "true" }
          : alt !== undefined
            ? { role: "img", "aria-label": alt }
            : {};

    return (
      <AvatarRoot {...rootRest} ref={ref}>
        {children ?? (
          <>
            {src !== undefined && alt !== undefined ? (
              <AvatarImage src={src} alt={alt} />
            ) : null}
            <AvatarFallback delay={fallbackDelay} {...fallbackA11y}>
              {fallback}
            </AvatarFallback>
          </>
        )}
      </AvatarRoot>
    );
  },
);
AvatarShorthand.displayName = "Avatar";

/* The public surface is the shorthand with the compound parts hung
 * off it as static members. This mirrors Dialog / Toggle / Tabs and
 * lets `<Avatar.Root>` / `<Avatar.Image>` / `<Avatar.Fallback>` work
 * directly while keeping the shorthand the 80%-case entry point. */
type AvatarCompound = typeof AvatarShorthand & {
  Root: typeof AvatarRoot;
  Image: typeof AvatarImage;
  Fallback: typeof AvatarFallback;
};

(AvatarShorthand as AvatarCompound).Root = AvatarRoot;
(AvatarShorthand as AvatarCompound).Image = AvatarImage;
(AvatarShorthand as AvatarCompound).Fallback = AvatarFallback;

export const Avatar = AvatarShorthand as AvatarCompound;
export { AvatarRoot, AvatarImage, AvatarFallback };
