/*
 * AppShell — the application frame composition.
 *
 * Canonical structure (this is the REQUIRED shape — `AppShell.Body` is
 * not optional: it is the `Split` row that holds the Sidebar rail + Main
 * and owns the `data-sidebar-open` collapse gate):
 *
 *   <AppShell sidebarOpen={open} onSidebarOpenChange={setOpen}>
 *     <AppShell.Header>
 *       <button onClick={() => setOpen((o) => !o)} aria-expanded={open}>
 *         Toggle sidebar
 *       </button>
 *       …brand / actions…
 *     </AppShell.Header>
 *     <AppShell.Body>
 *       <AppShell.Sidebar asChild>
 *         <nav aria-label="Primary">…nav…</nav>
 *       </AppShell.Sidebar>
 *       <AppShell.Main>…page…</AppShell.Main>
 *     </AppShell.Body>
 *     <AppShell.Footer>…footer…</AppShell.Footer>   {/* optional *​/}
 *   </AppShell>
 *
 * Sidebar + Main MUST be wrapped in `AppShell.Body`; placing them as bare
 * children of the root renders broken (no rail/main row, no collapse).
 * `AppShell.Footer` is optional.
 *
 * Structure — a full-height column:
 *   ┌──────────────────────────────────────────┐  Header  (<header>, raised
 *   │ Header                                    │           surface + hairline)
 *   ├──────────────┬───────────────────────────┤  body    (a `Split`:
 *   │ Sidebar rail │ Main (fluid)              │           Side rail + Main)
 *   ├──────────────┴───────────────────────────┤
 *   │ Footer (optional)                         │  Footer  (<footer>)
 *   └──────────────────────────────────────────┘
 *
 * We COMPOSE the `Split` primitive for the body (Sidebar = Split.Side,
 * Main = Split.Main) rather than re-rolling a rail/fluid flex row. The
 * root is a flex column so the Header/body/Footer bands stack; the body
 * `Split` flexes to fill the remaining block-size. The root sets
 * `min-block-size: 100dvh` so the shell fills the dynamic viewport height
 * (dvh, not px — it tracks mobile browser chrome show/hide; the
 * no-raw-px rule explicitly permits dvh/%). A consumer that mounts the
 * shell inside a smaller region can override via `style`/`className`.
 *
 * Sidebar collapse — CONTROLLED / uncontrolled, mirroring `Drawer`'s
 * open-state contract:
 *   - Pass `sidebarOpen` + `onSidebarOpenChange` to drive it (controlled).
 *   - Omit `sidebarOpen` and the shell holds its own state seeded from
 *     `defaultSidebarOpen` (default `true`); `onSidebarOpenChange` still
 *     fires so the consumer can observe toggles.
 * The toggle TRIGGER is the consumer's responsibility — there is NO
 * built-in hamburger. The consumer renders their own button inside
 * `AppShell.Header` content and flips `sidebarOpen` (controlled) or calls
 * `useAppShellSidebar().setOpen` (uncontrolled). This matches Drawer:
 * the surface exposes state + setter, the consumer owns the control.
 * When closed, the Sidebar collapses to zero inline-size and is removed
 * from the layout (`display: none`); Main spans the full width. The
 * collapse rides on `data-sidebar-open` data-attributes consumed by
 * AppShell.css, with a `prefers-reduced-motion` guard on the width
 * transition.
 *
 * a11y landmarks:
 *   - Header  → `<header>`  (banner)
 *   - Sidebar → `<aside>`   (complementary; `asChild` → `<nav>` when it
 *                            is the primary navigation landmark)
 *   - Main    → `<main>`    (the SINGLE main landmark of the document)
 *   - Footer  → `<footer>`  (contentinfo)
 * A skip-to-content link is BAKED IN as the first focusable child of the
 * shell: a `.zs-skip-link` anchor that is visually hidden until focused,
 * then appears pinned to the inline-start/top. Its `href` targets the
 * Main's `id` (generated with `useId`), so keyboard users can jump past
 * the header + sidebar straight to the page content. The Main `id` is
 * shared through context so `AppShell.Main` renders the matching `id`
 * automatically.
 *
 * AppShell MUST NOT set `data-theme` — the theme host is `<html>`, and
 * overlays portal to `<body>` and inherit from there. Setting a theme on
 * the shell would desync portalled overlays from the page.
 *
 * The root does NOT support `asChild`: it composes a fixed Header +
 * Split(body) + Footer structure, so a Slot would drop that body. The
 * parts (Sidebar) support `asChild` for the nav-landmark relevel.
 */
import {
  createContext,
  forwardRef,
  isValidElement,
  useCallback,
  useContext,
  useId,
  useMemo,
  useState,
  type ComponentPropsWithoutRef,
  type Ref,
} from "react";
import { Slot } from "../../components/_slot";
import { classnames } from "../../components/_classnames";
import { Split } from "../Split";
import type { Side } from "../_layout-primitives";

/** Which inline edge the sidebar rail sits on — the shared {@link Side}. */
export type AppShellSidebarSide = Side;

/* ─── context — Main id + sidebar state for parts/consumers ───────────── */

interface AppShellContextValue {
  /** Generated id wired onto Main + the skip link's `href`. */
  mainId: string;
  /** Current resolved sidebar-open state. */
  sidebarOpen: boolean;
  /** Setter that respects controlled/uncontrolled + fires the callback. */
  setSidebarOpen: (open: boolean) => void;
  /** Inline edge the rail sits on — flows into the body `Split side`. */
  sidebarSide: AppShellSidebarSide;
}

const AppShellContext = createContext<AppShellContextValue | null>(null);

/**
 * Access the shell's sidebar state + setter and the Main landmark id.
 * Lets a consumer drive the toggle from within the shell subtree when
 * running uncontrolled (no `sidebarOpen` prop).
 *
 * THROWS ("useAppShellSidebar must be used within an <AppShell>") when
 * called outside an `<AppShell>` provider. This is intentional and is the
 * idiomatic React context-hook pattern (it matches this repo's `useTheme`,
 * which throws "useTheme must be used within ThemeProvider"): a clear
 * throw surfaces a real consumer wiring bug immediately rather than
 * silently no-op'ing and hiding it. Do NOT change this to a silent
 * fallback.
 */
export function useAppShellSidebar(): AppShellContextValue {
  const ctx = useContext(AppShellContext);
  if (ctx == null) {
    throw new Error("useAppShellSidebar must be used within an <AppShell>.");
  }
  return ctx;
}

/* ─── props ───────────────────────────────────────────────────────────── */

export interface AppShellProps extends ComponentPropsWithoutRef<"div"> {
  /**
   * Controlled sidebar-open state. When provided, the shell does not
   * track its own state — drive it from the consumer and pair with
   * `onSidebarOpenChange`. Omit for uncontrolled (see
   * `defaultSidebarOpen`).
   */
  sidebarOpen?: boolean;
  /**
   * Initial open state when uncontrolled (no `sidebarOpen`). Default
   * `true` (sidebar visible on first paint).
   */
  defaultSidebarOpen?: boolean;
  /**
   * Fires whenever the sidebar should toggle. Always called (controlled
   * or not) so the consumer can observe state; in uncontrolled mode the
   * shell also updates its internal state.
   */
  onSidebarOpenChange?: (open: boolean) => void;
  /**
   * Inline-size of the sidebar rail when open (a free CSS length — this
   * is a structural rail width, the one place an explicit length is
   * expected). Default `"16rem"`.
   */
  sidebarWidth?: string;
  /**
   * Which inline edge the sidebar rail sits on. `start` (default) reads
   * left in LTR, right in RTL; `end` flips. Routed to the underlying
   * `Split side`.
   */
  sidebarSide?: AppShellSidebarSide;
  /**
   * Accessible label for the skip-to-content link. Defaults to
   * `"Skip to main content"`. Localized consumers override.
   */
  skipLinkLabel?: string;
}

/* ─── part props ──────────────────────────────────────────────────────── */

export type AppShellHeaderProps = ComponentPropsWithoutRef<"div">;
export type AppShellMainProps = ComponentPropsWithoutRef<"div">;
export type AppShellFooterProps = ComponentPropsWithoutRef<"div">;
export interface AppShellSidebarProps extends ComponentPropsWithoutRef<"div"> {
  /**
   * Render-as the single child element rather than the default
   * `<aside>`. Use to render a `<nav>` when the sidebar IS the primary
   * navigation landmark. Routed through `Slot` (React-19-safe refs).
   */
  asChild?: boolean;
}

/* ─── root ────────────────────────────────────────────────────────────── */

const AppShellRoot = forwardRef<HTMLDivElement, AppShellProps>(
  function AppShellRoot(
    {
      sidebarOpen: sidebarOpenProp,
      defaultSidebarOpen = true,
      onSidebarOpenChange,
      sidebarWidth = "16rem",
      sidebarSide = "start",
      skipLinkLabel = "Skip to main content",
      className,
      style,
      children,
      ...rest
    },
    ref,
  ) {
    // Controlled when `sidebarOpen` is supplied; otherwise the shell
    // tracks its own state seeded from `defaultSidebarOpen`. Mirrors
    // Drawer's open-state contract (Drawer delegates to Base UI; here we
    // own the controllable-state machinery directly).
    const isControlled = sidebarOpenProp !== undefined;
    const [uncontrolledOpen, setUncontrolledOpen] =
      useState(defaultSidebarOpen);
    const sidebarOpen = isControlled ? sidebarOpenProp : uncontrolledOpen;

    const setSidebarOpen = useCallback(
      (next: boolean) => {
        if (!isControlled) {
          setUncontrolledOpen(next);
        }
        onSidebarOpenChange?.(next);
      },
      [isControlled, onSidebarOpenChange],
    );

    // useId gives a stable, collision-free id for the Main landmark; the
    // skip link's href targets it (`#${mainId}`) and AppShell.Main
    // renders the matching id from context.
    const generatedId = useId();
    const mainId = `${generatedId}-main`;

    const ctx = useMemo<AppShellContextValue>(
      () => ({ mainId, sidebarOpen, setSidebarOpen, sidebarSide }),
      [mainId, sidebarOpen, setSidebarOpen, sidebarSide],
    );

    const composedClassName = classnames("zs-app-shell", className);

    const layoutVars: React.CSSProperties = {
      "--app-shell-sidebar-width": sidebarWidth,
      ...style,
    } as React.CSSProperties;

    // Canonical structure: Header / Body[Sidebar + Main] / Footer(opt).
    // `AppShell.Body` is REQUIRED — it is the Split row that owns the
    // sidebar/main layout + the collapse gate; Sidebar/Main are placed
    // inside it, not as bare root children. The skip link is injected as
    // the FIRST focusable child so Tab from the top of the document lands
    // on it before any header control.
    return (
      <AppShellContext.Provider value={ctx}>
        <div
          {...rest}
          ref={ref as Ref<HTMLDivElement>}
          data-slot="app-shell"
          data-sidebar-side={sidebarSide}
          data-sidebar-open={sidebarOpen ? "" : undefined}
          className={composedClassName}
          style={layoutVars}
        >
          <a className="zs-skip-link" href={`#${mainId}`}>
            {skipLinkLabel}
          </a>
          {children}
        </div>
      </AppShellContext.Provider>
    );
  },
);
AppShellRoot.displayName = "AppShell";

/* ─── Header ──────────────────────────────────────────────────────────── */

const AppShellHeader = forwardRef<HTMLElement, AppShellHeaderProps>(
  function AppShellHeader({ className, ...rest }, ref) {
    // Rest spread BEFORE internal data-slot so callers cannot overwrite
    // the documented contract attr via `{...rest}` (Card item).
    return (
      <header
        {...rest}
        ref={ref as Ref<HTMLElement>}
        data-slot="app-shell-header"
        className={classnames("zs-app-shell__header", className)}
      />
    );
  },
);
AppShellHeader.displayName = "AppShell.Header";

/* ─── Sidebar ─────────────────────────────────────────────────────────── */

const AppShellSidebar = forwardRef<HTMLElement, AppShellSidebarProps>(
  function AppShellSidebar(
    { asChild = false, className, children, ...rest },
    ref,
  ) {
    const ctx = useContext(AppShellContext);
    // Dev-mode parity with Card/Split: warn when asChild has no single
    // valid element child (Slot would render nothing silently). DCEs in
    // prod.
    if (process.env.NODE_ENV !== "production") {
      if (asChild && !isValidElement(children)) {
        // eslint-disable-next-line no-console
        console.warn(
          "AppShell.Sidebar asChild requires a single React element child; rendering nothing.",
        );
      }
    }
    // Compose AppShell.Sidebar as the Split.Side rail (the fixed-width
    // column). We route through `asChild` so Split.Side's Slot composes
    // its rail class onto OUR landmark element. The semantic
    // `data-slot="app-shell-sidebar"` is passed to Split.Side (the Slot
    // wrapper): Slot merges wrapper props OVER the child's, so the slot
    // owner is the wrapper — passing it there makes the rendered landmark
    // carry `app-shell-sidebar` rather than the primitive's `split-side`.
    // Our class / `data-sidebar-open` ride on the landmark element (the
    // CSS collapse keys off the body, but the state attr stays on the rail
    // for consumer hooks).
    const Comp = asChild ? Slot : "aside";
    return (
      <Split.Side asChild data-slot="app-shell-sidebar">
        <Comp
          {...rest}
          ref={ref as Ref<HTMLElement>}
          data-sidebar-open={ctx?.sidebarOpen ? "" : undefined}
          className={classnames("zs-app-shell__sidebar", className)}
        >
          {children}
        </Comp>
      </Split.Side>
    );
  },
);
AppShellSidebar.displayName = "AppShell.Sidebar";

/* ─── Main ────────────────────────────────────────────────────────────── */

const AppShellMain = forwardRef<HTMLElement, AppShellMainProps>(
  function AppShellMain({ className, id: idProp, children, ...rest }, ref) {
    const ctx = useContext(AppShellContext);
    // The Main carries the shell's GENERATED id (`ctx.mainId`) because the
    // baked-in skip link targets `#${ctx.mainId}`. The generated id is
    // applied AFTER `{...rest}` so a consumer-supplied `id` cannot override
    // it and silently break the skip link. Dev-warn (DCEs in prod) when a
    // consumer passes an `id` so the ignored value is diagnosable. Compose
    // Main as Split.Main (the fluid region) via asChild so there is no
    // wrapper div between the body Split and the <main>.
    if (process.env.NODE_ENV !== "production") {
      if (idProp != null) {
        // eslint-disable-next-line no-console
        console.warn(
          "AppShell.Main id is managed for the skip link; the provided id was ignored.",
        );
      }
    }
    return (
      <Split.Main
        data-slot="app-shell-main"
        className={classnames("zs-app-shell__main", className)}
        asChild
      >
        <main {...rest} ref={ref as Ref<HTMLElement>} id={ctx?.mainId}>
          {children}
        </main>
      </Split.Main>
    );
  },
);
AppShellMain.displayName = "AppShell.Main";

/* ─── Footer ──────────────────────────────────────────────────────────── */

const AppShellFooter = forwardRef<HTMLElement, AppShellFooterProps>(
  function AppShellFooter({ className, ...rest }, ref) {
    return (
      <footer
        {...rest}
        ref={ref as Ref<HTMLElement>}
        data-slot="app-shell-footer"
        className={classnames("zs-app-shell__footer", className)}
      />
    );
  },
);
AppShellFooter.displayName = "AppShell.Footer";

/* ─── Body — composes Split for the sidebar/main division ─────────────── */

/**
 * Props for `AppShell.Body` — the REQUIRED row that holds the Sidebar
 * rail and Main. Composes the `Split` primitive and owns the rail
 * width/side (wired from the root) plus the `data-sidebar-open` collapse
 * gate. Sidebar + Main MUST be nested inside `AppShell.Body`; placing
 * them as bare root children renders broken.
 */
export type AppShellBodyProps = ComponentPropsWithoutRef<"div">;

const AppShellBody = forwardRef<HTMLDivElement, AppShellBodyProps>(
  function AppShellBody({ className, children, ...rest }, ref) {
    const ctx = useContext(AppShellContext);
    // Compose the Split primitive directly (the sidebar rail + main row).
    // Split owns the row layout + its own `data-slot="split"`; our
    // `data-sidebar-open` rides through the prop spread (Split doesn't
    // touch it) so the CSS collapse rule
    // `.zs-app-shell__body:not([data-sidebar-open])` keys off it, and our
    // `zs-app-shell__body` class composes via Split's `classnames`.
    return (
      <Split
        {...rest}
        ref={ref}
        data-slot="app-shell-body"
        data-sidebar-open={ctx?.sidebarOpen ? "" : undefined}
        className={classnames("zs-app-shell__body", className)}
        side={ctx?.sidebarSide ?? "start"}
        sideWidth="var(--app-shell-sidebar-width, 16rem)"
      >
        {children}
      </Split>
    );
  },
);
AppShellBody.displayName = "AppShell.Body";

/* ─── public namespace ────────────────────────────────────────────────── */

type AppShellComponent = typeof AppShellRoot & {
  Header: typeof AppShellHeader;
  Sidebar: typeof AppShellSidebar;
  Body: typeof AppShellBody;
  Main: typeof AppShellMain;
  Footer: typeof AppShellFooter;
};

export const AppShell = AppShellRoot as AppShellComponent;
AppShell.Header = AppShellHeader;
AppShell.Sidebar = AppShellSidebar;
AppShell.Body = AppShellBody;
AppShell.Main = AppShellMain;
AppShell.Footer = AppShellFooter;
