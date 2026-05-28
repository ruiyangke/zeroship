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

/* ─── Avatar.Image ──────────────────────────────────────────────────── */

export interface AvatarImageProps
  extends Omit<BaseImageProps, "className" | "render"> {
  /**
   * Optional class hook on the underlying `<img>`. The default class
   * paints the image flush against the Root's content-box at the
   * Root's `border-radius`.
   */
  className?: string;
}

const AvatarImage = forwardRef<HTMLImageElement, AvatarImageProps>(
  function AvatarImage({ className, ...rest }, ref) {
    return (
      <BaseAvatar.Image
        {...(rest as BaseImageProps)}
        ref={ref}
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
 */
export interface AvatarProps
  extends Omit<AvatarRootProps, "children"> {
  /** Image source — when set, mounts Avatar.Image inside the lockup. */
  src?: string;
  /**
   * Alt text for the image. REQUIRED when `src` is set. Pass `alt=""`
   * for decorative images (AT will skip the node). A dev-warn fires
   * once per `src` when `src` is set and `alt` is omitted.
   */
  alt?: string;
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

const AvatarShorthand = forwardRef<HTMLSpanElement, AvatarProps>(
  function Avatar(
    { src, alt, fallback, fallbackDelay, children, ...rootRest },
    ref,
  ) {
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

    return (
      <AvatarRoot {...rootRest} ref={ref}>
        {children ?? (
          <>
            {src ? <AvatarImage src={src} alt={alt} /> : null}
            <AvatarFallback delay={fallbackDelay}>{fallback}</AvatarFallback>
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
