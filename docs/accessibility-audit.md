# Accessibility audit — zeroship-builder

Date: 2026-05-01
Scope: `apps/zeroship-builder` (creator dashboard / builder UI). End-user
apps shipped on the runtime have their own WCAG story owned by the
generated code; this doc covers what the platform itself ships.

Target: **WCAG 2.1 Level AA**.

---

## Method

- Manual audit of every interactive element, list-rendering surface,
  and modal flow.
- Keyboard-only walkthrough of the public marketing → signup → wizard
  → workspace pipeline.
- Screen-reader spot-check of the workspace (VoiceOver on macOS).
- Color-contrast computation against the design tokens in
  `src/client/index.css` (paper ink, paper-2 ink, paper tomato,
  ivy-3 ivy, etc.).

The full Lighthouse / axe-core sweep is left for a CI integration in a
follow-up; this audit is the human pass that should make those
automated runs uneventful.

---

## What's in place after this pass

### Error containment

- `ErrorBoundary` mounted at the React root in `main.tsx` so a render
  crash anywhere lands on an editorial wall instead of a blank white
  page.
- Per-canvas `ErrorBoundary` wrappers inside `WorkspaceShell` so a
  single canvas crash doesn't blank the whole workspace; the chat rail
  keeps working and the user can switch tabs out of the broken one.

### Focus

- Global `:focus-visible` rule in `index.css` paints a 2px tomato
  outline with 2px offset on every interactive element on keyboard
  focus, suppressed for form inputs (their `focus:border-ink` already
  gives a visible state and a doubled ring breaks the editorial
  baseline).
- Inputs / textareas / selects opt out of the global ring and use
  their existing `focus:border-ink` border-shift instead.
- TopBar links (logo, account, project URL pill), workspace canvas
  pills, in-canvas action buttons all carry explicit
  `focus:outline-2 focus:outline-tomato focus:outline-offset-2`
  classes for consistency above the global rule.
- `Modal` now captures `document.activeElement` on open and restores
  focus on close (WCAG 2.4.3 — focus order). It also auto-focuses the
  first focusable element inside the dialog (or the dialog itself
  when it has none).

### Semantics & labels

- `Modal` uses `role="dialog"`, `aria-modal="true"`, and
  `aria-labelledby` against the rendered title id (or `aria-label`
  fallback when title is omitted).
- Icon-only buttons (TopBar tour, TopBar chat-toggle, TopBar
  account, LiveBanner dismiss, Toast dismiss, ChatComposer attach,
  composer attachment-remove, env-row delete, MessageActions
  copy/regenerate/edit, chat-drawer close) all carry `aria-label`.
- TopBar logo link has `aria-label="zeroship home"`.
- Workspace TopBar URL pill has descriptive `aria-label` even when
  hidden visually below `sm`.
- Form inputs use native `<label>` wrappers (Login / Signup /
  ForgotPassword / Account / Env / Plan-new-issue modal) — labels are
  associated by DOM containment, not just placeholder text.

### Keyboard

- `Modal` closes on `Escape`.
- `ChatComposer` ⌘/Ctrl + Enter to send; plain Enter for newline;
  `@`-mention dropdown navigates with ArrowUp/ArrowDown, commits on
  Enter/Tab, dismisses on Escape.
- Tab order follows DOM order in every shell — no `tabindex` overrides
  outside the Modal's `tabindex="-1"` for the focus container.

### Reduced motion

- The `prefers-reduced-motion: reduce` block in `index.css` already
  zeros out animation/transition durations globally. The new chat
  drawer and ErrorBoundary fallback inherit this for free — neither
  animates outside the existing transition tokens.

### Empty states

- Every list-rendering surface has an editorial empty state copy that
  explains what fills the slot. No `useQuery` site renders a blank
  rectangle when its data resolves to `[]`. Filter-no-match cases
  (Templates, Skills) get a "Show all" reset.

### Responsive

- Phone breakpoint (<768px): chat sidebar collapses out of the
  workspace grid and re-mounts as a full-screen drawer toggleable
  from a TopBar chat icon. All canvases respect available width.
- Tablet (768–1023px): keeps sidebar; canvases reflow into 1- or 2-
  column layouts as appropriate.
- Desktop (1024px+): full design intent — 3-column FilesCanvas,
  marginalia rail, etc.

### Color contrast

Computed against the OKLCH tokens in `index.css`:

| Foreground | Background | Ratio | Verdict |
| --- | --- | --- | --- |
| ink (oklch 0.18) | paper (0.97) | ~14.5:1 | AAA |
| ink-soft (0.40) | paper (0.97) | ~7.4:1 | AAA |
| pencil (0.55) | paper (0.97) | ~4.6:1 | AA (large text) |
| tomato (0.61) | paper (0.97) | ~3.7:1 | AA only at ≥18pt or for non-text UI |
| ivy (0.58) | paper (0.97) | ~4.0:1 | AA at ≥18pt |
| paper (0.97) | ink (0.18) | ~14.5:1 | AAA (LiveBanner, account dot) |

Tomato is reserved for primary action surfaces (StampButton, accent
underlines, status pulse) and headlines where its size puts it in
the "large text" category — AA compliant in all current uses. Pencil
is reserved for italic-serif metadata and never carries critical info.

### Known caveats / deferred

- No focus trap inside `Modal` (Tab can leave the dialog). The drawer
  on phone has the same caveat. Both are OK at AA — the modal is
  shallow, escape-closable, and focus restores. A `focus-trap-react`-
  style helper is the right tool when we add deeper modals.
- `ProductTour` overlay focus management not audited in this pass.
- No automated axe-core run in CI yet; following up under a separate
  ISSUE.
- LiveBanner copy button is icon+label but the icon-only ✕ dismiss
  is the only true icon-only control in that component (already
  labeled).

---

## Quick reference for new code

When you write a new interactive element:

1. **Buttons** with text content need no `aria-label`. Icon-only
   buttons must have one.
2. **Inputs** wrap inside a `<label>` (DOM containment associates
   them automatically) or use `aria-labelledby` against a sibling.
3. **Links** that are icon-only need `aria-label`; text links don't.
4. **Modals / dialogs** use the existing `Modal` component — it
   handles focus and keyboard already.
5. **Empty states** for list views: render an editorial italic-serif
   sentence in a dashed-border container; never an empty box.
6. **New keyframed animation**: nothing extra needed — the global
   `prefers-reduced-motion` block already disables transitions.
