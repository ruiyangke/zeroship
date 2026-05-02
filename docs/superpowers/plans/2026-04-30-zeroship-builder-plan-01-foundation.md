# zeroship-builder Plan 01 — Foundation (Workspace Shell + Chat UI)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the current `apps/zeroship-builder/` UI with a clean workspace shell and a chat surface that streams from a *mock* server function. End state: dev server starts, user opens the app, sees the new design, types a prompt, watches a streamed mock response with a fake tool receipt. **No real LLM yet** — that comes in Plan 02.

**Architecture:** Vite + React 19 client. Server functions via `@zeroship/vite-plugin` ("use server" modules compiled into chunked RPCs). Chat surface uses Vercel AI SDK's `@ai-sdk/react` (`useChat` hook) over an SSE-emitting server function. The server function is a mock that produces realistic-looking AI SDK protocol chunks (text deltas + tool calls + custom data parts) so the entire client UI is exercisable without an LLM.

**Tech Stack:** React 19 · TypeScript · Vite · Tailwind v4 · `@ai-sdk/react` (client) · `react-router-dom` · `react-markdown` + `remark-gfm` + `shiki` (markdown/code) · zod (types) · Playwright (e2e).

**Spec sections covered:** §1 foundation decisions · §3 IA · §4 design system · §9.1 preview canvas (placeholder) · §10 chat surface (composer, message list, receipts, mock data parts) · §25 responsive layouts (desktop default; mobile/tablet stubbed for Plan 11) · partial §8.2 (chat-as-wizard default flow without real agent yet).

**What's deliberately out of scope (deferred):**
- Real LLM / deepagents / Critic loop → Plan 02
- Auth, signup, login → Plan 03
- Project gallery, create-project flow → Plan 03
- All other canvases (files / data / media / logs / env / settings / plan / health) → Plans 03–06
- PM agent, SRE agent, skills, themes, feature sets → Plans 05–07
- Marketing / pricing → Plan 08
- Mobile/tablet polish (we ship desktop + a mobile fallback that says "best on desktop" for now) → Plan 11

---

## File Structure

### Files we CREATE in this plan

```
apps/zeroship-builder/
├── package.json                                  (rewritten — see Task 1)
├── vite.config.ts                                (kept; minor tweaks)
├── tsconfig.json                                 (kept)
├── index.html                                    (kept)
└── src/
    ├── client/
    │   ├── main.tsx                              (kept; minor tweaks for new App)
    │   ├── App.tsx                               (rewritten — minimal shell+chat for now)
    │   ├── index.css                             (rewritten — new design tokens)
    │   ├── lib/
    │   │   └── utils.ts                          (kept; cn helper)
    │   ├── design/                               (NEW)
    │   │   └── tokens.ts                         (TS-side access to CSS tokens)
    │   ├── components/                           (NEW; replaces existing components/)
    │   │   ├── Button.tsx                        (replaces StampButton + GhostButton)
    │   │   ├── Pill.tsx                          (replaces FilterPill)
    │   │   ├── Spinner.tsx
    │   │   ├── Toast.tsx
    │   │   ├── Modal.tsx
    │   │   ├── EmptyState.tsx
    │   │   ├── ErrorState.tsx
    │   │   └── Skeleton.tsx
    │   ├── workspace/                            (NEW)
    │   │   ├── WorkspaceShell.tsx                (TopBar + canvas + chat rail layout)
    │   │   ├── TopBar.tsx
    │   │   ├── CanvasPills.tsx
    │   │   ├── PreviewCanvasStub.tsx             (placeholder canvas)
    │   │   └── chat/
    │   │       ├── ChatRail.tsx                  (the right rail container)
    │   │       ├── ChatComposer.tsx              (textarea + send/stop + image input)
    │   │       ├── ChatMessages.tsx              (scrollable message list)
    │   │       ├── MessageUser.tsx               (user turn rendering)
    │   │       ├── MessageAssistant.tsx          (assistant turn rendering)
    │   │       ├── Receipt.tsx                   (tool-call receipt)
    │   │       ├── DiffCard.tsx                  (code-change card; placeholder render in Plan 01)
    │   │       ├── SurveyCard.tsx                (renders Survey data parts; placeholder in Plan 01)
    │   │       └── CriticRoundCard.tsx           (renders critic-round data parts; placeholder)
    │   └── types/
    │       └── chat.ts                           (Survey / Diff / CriticRound TypeScript types)
    └── server/
        ├── chat.ts                               ("use server" — the mock chat fn for Plan 01)
        └── _shared/
            └── stream.ts                         (helpers to emit AI SDK stream chunks)

apps/zeroship-builder/e2e/
└── chat-mock.spec.ts                             (NEW — Playwright e2e for the mock chat loop)
```

### Files we DELETE in this plan

The existing UI is being replaced entirely. These are the obvious deletes (verify with `git status` after Task 5):

```
src/client/components/FilterPill.tsx
src/client/components/GhostButton.tsx
src/client/components/Kpi.tsx
src/client/components/LedgerRow.tsx
src/client/components/LiveBanner.tsx
src/client/components/Marginalia.tsx
src/client/components/NotebookPrompt.tsx          (kept conceptually; rewritten in Plan 03)
src/client/components/PageFrame.tsx               (rewritten in Plan 03)
src/client/components/ProjectCard.tsx             (rewritten in Plan 03)
src/client/components/Receipt.tsx                 (rewritten — same name new file in chat/)
src/client/components/StampButton.tsx
src/client/components/TemplateCard.tsx
src/client/components/TopBar.tsx                  (rewritten — same name new file in workspace/)
src/client/auth/                                  (rewritten in Plan 03)
src/client/admin/                                 (rewritten in Plan 10)
src/client/builder/                               (rewritten across Plans 02 / 05 / 06)
src/client/api/                                   (rewritten in Plan 03)
src/client/pages/                                 (rewritten in Plan 03 + Plan 08)
src/client/workspace/components/                  (rewritten across plans)
src/client/workspace/tabs/                        (rewritten across Plans 03 / 04 / 05 / 06)
src/client/workspace/ChatRail.tsx                 (replaced by chat/ChatRail.tsx)
src/client/workspace/ProjectWorkspace.tsx         (replaced by WorkspaceShell.tsx)
```

We're not actually deleting these in this plan — we're **moving the new code in** while leaving old files in place, and the new App.tsx routes only the new shell. The old files become unreachable. Plan 03+ removes them as those flows get rebuilt.

---

## Task 1: Reset `package.json` to the chosen stack

**Files:**
- Modify: `apps/zeroship-builder/package.json`

This is a one-shot rewrite. Per design §4.8.7, we add AI SDK client deps, markdown/code rendering deps, and remove unused stuff.

- [ ] **Step 1: Read current package.json**

```bash
cat apps/zeroship-builder/package.json
```

Expected: see existing deps. Confirm presence of `@langchain/*`, `deepagents`, `@radix-ui/*`, `@uiw/react-codemirror`, `class-variance-authority`, `lucide-react`, `unenv`.

- [ ] **Step 2: Replace with the new package.json**

Write `apps/zeroship-builder/package.json` with the following content:

```json
{
  "name": "zeroship-builder",
  "private": true,
  "version": "0.2.0",
  "type": "module",
  "description": "The zeroship AI builder — runs ON zeroship itself.",
  "scripts": {
    "dev": "vite",
    "build": "tsc -b && vite build",
    "preview": "vite preview",
    "deploy": "vite build && zeroship deploy",
    "test:e2e": "playwright test",
    "test:e2e:ui": "playwright test --ui",
    "test:e2e:headed": "playwright test --headed"
  },
  "dependencies": {
    "@ai-sdk/react": "^1.0.0",
    "ai": "^4.0.0",
    "@codemirror/autocomplete": "^6.20.1",
    "@codemirror/commands": "^6.10.3",
    "@codemirror/lang-css": "^6.3.1",
    "@codemirror/lang-html": "^6.4.11",
    "@codemirror/lang-javascript": "^6.2.5",
    "@codemirror/lang-json": "^6.0.0",
    "@codemirror/language": "^6.12.3",
    "@codemirror/search": "^6.7.0",
    "@codemirror/state": "^6.6.0",
    "@codemirror/theme-one-dark": "^6.1.3",
    "@codemirror/view": "^6.41.1",
    "@langchain/anthropic": "^1.3.28",
    "@langchain/core": "^1.1.42",
    "@langchain/langgraph": "^1.2.9",
    "@langchain/openai": "^1.4.5",
    "@tailwindcss/vite": "^4.2.2",
    "@tanstack/react-query": "^5.96.1",
    "@uiw/react-codemirror": "^4.25.9",
    "@zeroship/vite-plugin": "file:../../sdks/vite-plugin",
    "clsx": "^2.1.1",
    "deepagents": "^1.9.0",
    "react": "^19.2.4",
    "react-dom": "^19.2.4",
    "react-markdown": "^9.0.0",
    "react-router-dom": "^7.13.2",
    "remark-gfm": "^4.0.0",
    "shiki": "^1.0.0",
    "tailwind-merge": "^3.5.0",
    "tailwindcss": "^4.2.2",
    "zod": "^4.3.6"
  },
  "devDependencies": {
    "@playwright/test": "^1.59.1",
    "@types/node": "^22.19.17",
    "@types/react": "^19.2.14",
    "@types/react-dom": "^19.2.3",
    "@vitejs/plugin-react": "^6.0.1",
    "typescript": "~5.9.3",
    "vite": "^8.0.1"
  }
}
```

Notes on the diff vs current:
- **Added**: `@ai-sdk/react`, `ai`, `react-markdown`, `remark-gfm`, `shiki`
- **Removed**: `@radix-ui/*` (we'll build our own primitives), `class-variance-authority` (one helper instead), `lucide-react` (very few icons), `unenv` (no longer needed; runtime has node-compat)
- **Moved**: `@vitejs/plugin-react` from `dependencies` to `devDependencies` (correct location)
- **Kept**: `@codemirror/*` for now — Plan 04 decides between Monaco and CodeMirror for the editor

`@langchain/*` and `deepagents` stay even though Plan 01 doesn't use them — Plan 02 does, and we'd rather keep the dep tree stable across plans.

- [ ] **Step 3: Reinstall**

```bash
cd apps/zeroship-builder && rm -rf node_modules package-lock.json && npm install
```

Expected: clean install with no peer-dep warnings. `npm ls @ai-sdk/react` should print a single tree with no UNMET dependencies.

- [ ] **Step 4: Verify dev server still starts**

```bash
cd apps/zeroship-builder && npm run dev &
sleep 3
curl -s http://localhost:5173 | head -20
```

Expected: `<!DOCTYPE html>` plus the Vite dev script tags. Kill with `kill %1`.

- [ ] **Step 5: Commit**

```bash
git add apps/zeroship-builder/package.json apps/zeroship-builder/package-lock.json
git commit -m "builder: reset deps to AI-SDK-on-client stack"
```

---

## Task 2: New design tokens (`index.css`)

**Files:**
- Modify: `apps/zeroship-builder/src/client/index.css`
- Create: `apps/zeroship-builder/src/client/design/tokens.ts`

Per design §4.2 (color), §4.3 (type), §4.4 (motion). The tokens are CSS custom properties; a tiny TS module re-exports them as constants for components that need values in code.

- [ ] **Step 1: Replace `src/client/index.css`**

```css
@import url("https://fonts.googleapis.com/css2?family=Fraunces:ital,opsz,wght@0,9..144,300..800;1,9..144,300..800&family=Source+Serif+4:ital,opsz,wght@0,8..60,200..900;1,8..60,200..900&family=Inter:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500&display=swap");
@import "tailwindcss";

@theme {
  /* paper / ink / pencil */
  --color-paper:   oklch(0.97 0.012 89);
  --color-paper-2: oklch(0.94 0.014 86);
  --color-paper-3: oklch(0.91 0.014 84);
  --color-ink:     oklch(0.18 0.013 60);
  --color-ink-soft:oklch(0.40 0.013 65);
  --color-pencil:  oklch(0.55 0.012 70);
  --color-rule:    oklch(0.78 0.014 75);
  --color-rule-2:  oklch(0.85 0.014 78);

  /* semantic colors — six distinct jobs (per §4.2) */
  --color-tomato:    oklch(0.61 0.21 27);   /* primary action only */
  --color-tomato-2:  oklch(0.55 0.21 25);
  --color-tomato-3:  oklch(0.90 0.06 27);
  --color-ivy:       oklch(0.58 0.16 152);  /* success / live / healthy */
  --color-ivy-2:     oklch(0.50 0.16 150);
  --color-ivy-3:     oklch(0.93 0.05 150);
  --color-cobalt:    oklch(0.55 0.18 252);  /* info / link */
  --color-amber:     oklch(0.72 0.15 80);   /* warning */
  --color-blood:     oklch(0.50 0.21 28);   /* destructive */

  /* type */
  --font-display: "Fraunces", "Times New Roman", serif;
  --font-serif:   "Source Serif 4", Georgia, serif;
  --font-sans:    "Inter", system-ui, sans-serif;
  --font-mono:    "JetBrains Mono", ui-monospace, monospace;
}

html, body, #root { height: 100%; }
body {
  background: var(--color-paper);
  color: var(--color-ink);
  font-family: var(--font-sans);     /* operational default = sans */
  font-size: 14px;
  line-height: 1.5;
  -webkit-font-smoothing: antialiased;
}

::selection { background: var(--color-tomato); color: var(--color-paper); }

/* hairline rule */
.hairline { height: 1px; background: var(--color-rule); border: 0; }

/* spinning loader */
@keyframes spin { to { transform: rotate(360deg); } }
.spin { animation: spin 0.9s linear infinite; }

/* live pulse — used only on green ivy dots */
@keyframes pulse-ivy {
  0%, 100% { box-shadow: 0 0 0 0 rgba(38, 166, 116, 0.5); }
  50%      { box-shadow: 0 0 0 7px rgba(38, 166, 116, 0); }
}
.pulse-dot { animation: pulse-ivy 1.5s ease-in-out infinite; }

/* respect reduced motion */
@media (prefers-reduced-motion: reduce) {
  *, *::before, *::after {
    animation-duration: 0.01ms !important;
    transition-duration: 0.01ms !important;
  }
}
```

- [ ] **Step 2: Create `src/client/design/tokens.ts`**

```ts
// CSS-level tokens are the source of truth (index.css). This file re-exports
// them as TypeScript constants for places where we need raw values in JS
// (canvas drawing, third-party lib config, etc.). Keep in sync with index.css.

export const colors = {
  paper:    "oklch(0.97 0.012 89)",
  paper2:   "oklch(0.94 0.014 86)",
  paper3:   "oklch(0.91 0.014 84)",
  ink:      "oklch(0.18 0.013 60)",
  inkSoft:  "oklch(0.40 0.013 65)",
  pencil:   "oklch(0.55 0.012 70)",
  rule:     "oklch(0.78 0.014 75)",
  rule2:    "oklch(0.85 0.014 78)",
  tomato:   "oklch(0.61 0.21 27)",
  tomato2:  "oklch(0.55 0.21 25)",
  tomato3:  "oklch(0.90 0.06 27)",
  ivy:      "oklch(0.58 0.16 152)",
  ivy2:     "oklch(0.50 0.16 150)",
  ivy3:     "oklch(0.93 0.05 150)",
  cobalt:   "oklch(0.55 0.18 252)",
  amber:    "oklch(0.72 0.15 80)",
  blood:    "oklch(0.50 0.21 28)",
} as const;

export const fonts = {
  display: '"Fraunces", "Times New Roman", serif',
  serif:   '"Source Serif 4", Georgia, serif',
  sans:    '"Inter", system-ui, sans-serif',
  mono:    '"JetBrains Mono", ui-monospace, monospace',
} as const;

export const motion = {
  ease: "cubic-bezier(.2, .7, .2, 1)",
  durations: { fast: 120, base: 200, slow: 380 },
} as const;
```

- [ ] **Step 3: Run tsc; no errors expected**

```bash
cd apps/zeroship-builder && npx tsc --noEmit
```

Expected: clean (no errors). If you see errors about missing `@types/*`, run `npm install` again.

- [ ] **Step 4: Commit**

```bash
git add apps/zeroship-builder/src/client/index.css apps/zeroship-builder/src/client/design/
git commit -m "builder: install new design tokens (colors, type, motion)"
```

---

## Task 3: `<Button>` primitive (replaces StampButton + GhostButton)

**Files:**
- Create: `apps/zeroship-builder/src/client/components/Button.tsx`
- Test: `apps/zeroship-builder/src/client/components/Button.test.tsx` (exists if vitest is set up; otherwise skip — Playwright e2e covers it)

Per design §4.5 component library.

- [ ] **Step 1: Create `Button.tsx`**

```tsx
import type { ButtonHTMLAttributes, ReactNode } from "react";
import { cn } from "../lib/utils";

type Variant = "primary" | "secondary" | "ghost" | "destructive" | "link";
type Size = "sm" | "md" | "lg";

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant;
  size?: Size;
  loading?: boolean;
  leadingIcon?: ReactNode;
  trailingIcon?: ReactNode;
}

const VARIANT_CLASSES: Record<Variant, string> = {
  primary:
    "bg-tomato text-paper border-0 hover:bg-tomato-2 active:translate-y-px " +
    "shadow-[0_2px_0_-1px_var(--color-tomato-2)]",
  secondary:
    "bg-paper-2 text-ink border border-rule hover:border-ink",
  ghost:
    "bg-transparent text-ink-soft border border-rule hover:border-ink hover:text-ink",
  destructive:
    "bg-blood text-paper border-0 hover:opacity-90 active:translate-y-px",
  link:
    "bg-transparent text-cobalt border-0 underline-offset-2 hover:underline px-0 py-0",
};

const SIZE_CLASSES: Record<Size, string> = {
  sm: "px-3 py-1.5 text-xs",
  md: "px-4 py-2 text-sm",
  lg: "px-5 py-2.5 text-base",
};

export function Button({
  variant = "primary",
  size = "md",
  loading,
  disabled,
  leadingIcon,
  trailingIcon,
  className,
  children,
  ...rest
}: ButtonProps) {
  return (
    <button
      {...rest}
      disabled={disabled || loading}
      className={cn(
        "inline-flex items-center gap-2 font-sans font-medium",
        "transition-[transform,opacity,background-color,border-color] duration-200",
        "disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer rounded",
        VARIANT_CLASSES[variant],
        SIZE_CLASSES[size],
        className,
      )}
    >
      {loading ? (
        <span
          className="inline-block size-3.5 rounded-full border-2 border-current border-t-transparent spin"
          aria-hidden="true"
        />
      ) : leadingIcon}
      {children}
      {trailingIcon}
    </button>
  );
}
```

- [ ] **Step 2: Verify it compiles**

```bash
cd apps/zeroship-builder && npx tsc --noEmit
```

Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add apps/zeroship-builder/src/client/components/Button.tsx
git commit -m "builder: add Button primitive (5 variants, 3 sizes)"
```

---

## Task 4: `<Pill>` primitive (replaces FilterPill, used for canvas pills)

**Files:**
- Create: `apps/zeroship-builder/src/client/components/Pill.tsx`

- [ ] **Step 1: Create `Pill.tsx`**

```tsx
import type { ButtonHTMLAttributes, ReactNode } from "react";
import { cn } from "../lib/utils";

export interface PillProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  active?: boolean;
  size?: "sm" | "md";
  leadingIcon?: ReactNode;
}

export function Pill({
  active,
  size = "md",
  leadingIcon,
  className,
  children,
  ...rest
}: PillProps) {
  return (
    <button
      {...rest}
      className={cn(
        "inline-flex items-center gap-1.5 rounded-full font-sans transition-colors cursor-pointer",
        size === "sm" ? "px-2.5 py-1 text-[11px]" : "px-3 py-1.5 text-xs",
        active
          ? "bg-tomato text-paper border-0"
          : "bg-transparent text-ink-soft border border-rule hover:border-ink hover:text-ink",
        className,
      )}
    >
      {leadingIcon}
      {children}
    </button>
  );
}
```

- [ ] **Step 2: Verify**

```bash
cd apps/zeroship-builder && npx tsc --noEmit
```

Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add apps/zeroship-builder/src/client/components/Pill.tsx
git commit -m "builder: add Pill primitive"
```

---

## Task 5: `<Spinner>`, `<EmptyState>`, `<ErrorState>`, `<Skeleton>`, `<Toast>`, `<Modal>` primitives

These are small. One commit each.

**Files:**
- Create: `apps/zeroship-builder/src/client/components/Spinner.tsx`
- Create: `apps/zeroship-builder/src/client/components/EmptyState.tsx`
- Create: `apps/zeroship-builder/src/client/components/ErrorState.tsx`
- Create: `apps/zeroship-builder/src/client/components/Skeleton.tsx`
- Create: `apps/zeroship-builder/src/client/components/Toast.tsx`
- Create: `apps/zeroship-builder/src/client/components/Modal.tsx`

- [ ] **Step 1: `Spinner.tsx`**

```tsx
import { cn } from "../lib/utils";

export function Spinner({ size = 16, className }: { size?: number; className?: string }) {
  return (
    <span
      role="status"
      aria-label="Loading"
      style={{ width: size, height: size }}
      className={cn(
        "inline-block rounded-full border-2 border-current border-t-transparent spin",
        className,
      )}
    />
  );
}
```

- [ ] **Step 2: `EmptyState.tsx`**

```tsx
import type { ReactNode } from "react";

export interface EmptyStateProps {
  title: string;
  description?: string;
  action?: ReactNode;
}

export function EmptyState({ title, description, action }: EmptyStateProps) {
  return (
    <div className="flex flex-col items-center justify-center py-12 px-6 text-center">
      <h3 className="font-display text-2xl font-medium text-ink mb-1">{title}</h3>
      {description && (
        <p className="font-serif text-sm text-ink-soft max-w-md mb-4">{description}</p>
      )}
      {action}
    </div>
  );
}
```

- [ ] **Step 3: `ErrorState.tsx`**

```tsx
import type { ReactNode } from "react";
import { Button } from "./Button";

export interface ErrorStateProps {
  message: string;
  onRetry?: () => void;
  retryLabel?: string;
  detail?: ReactNode;
}

export function ErrorState({ message, onRetry, retryLabel = "Retry", detail }: ErrorStateProps) {
  return (
    <div role="alert" className="border border-blood/30 bg-blood/5 px-4 py-3 rounded">
      <div className="font-sans text-sm text-blood mb-1">{message}</div>
      {detail && <div className="font-mono text-[11px] text-ink-soft mb-2">{detail}</div>}
      {onRetry && (
        <Button variant="ghost" size="sm" onClick={onRetry}>
          {retryLabel}
        </Button>
      )}
    </div>
  );
}
```

- [ ] **Step 4: `Skeleton.tsx`**

```tsx
import { cn } from "../lib/utils";

export function Skeleton({ className }: { className?: string }) {
  return (
    <div
      aria-hidden="true"
      className={cn(
        "bg-paper-3 rounded animate-pulse",
        className,
      )}
    />
  );
}
```

- [ ] **Step 5: `Toast.tsx` (minimal — full toast system in Plan 03)**

```tsx
import type { ReactNode } from "react";
import { cn } from "../lib/utils";

export type ToastTone = "info" | "success" | "warn" | "error";

export interface ToastProps {
  tone?: ToastTone;
  children: ReactNode;
  onDismiss?: () => void;
}

const TONE_CLASSES: Record<ToastTone, string> = {
  info:    "border-cobalt/30 bg-cobalt/5  text-ink",
  success: "border-ivy/30    bg-ivy/5     text-ink",
  warn:    "border-amber/40  bg-amber/5   text-ink",
  error:   "border-blood/30  bg-blood/5   text-blood",
};

export function Toast({ tone = "info", children, onDismiss }: ToastProps) {
  return (
    <div
      role="status"
      className={cn(
        "border px-4 py-2.5 rounded font-sans text-sm flex items-center gap-3",
        TONE_CLASSES[tone],
      )}
    >
      <div className="flex-1">{children}</div>
      {onDismiss && (
        <button
          type="button"
          onClick={onDismiss}
          aria-label="Dismiss"
          className="text-current opacity-60 hover:opacity-100 cursor-pointer"
        >
          ✕
        </button>
      )}
    </div>
  );
}
```

- [ ] **Step 6: `Modal.tsx` (minimal)**

```tsx
import { useEffect, type ReactNode } from "react";

export interface ModalProps {
  open: boolean;
  onClose: () => void;
  title?: string;
  children: ReactNode;
  width?: number;
}

export function Modal({ open, onClose, title, children, width = 480 }: ModalProps) {
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  if (!open) return null;
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={title}
      className="fixed inset-0 z-50 flex items-center justify-center bg-ink/40"
      onClick={onClose}
    >
      <div
        onClick={(e) => e.stopPropagation()}
        style={{ width }}
        className="bg-paper border border-rule rounded shadow-2xl max-w-[calc(100vw-32px)] max-h-[calc(100vh-64px)] overflow-auto"
      >
        {title && (
          <div className="px-5 py-3 border-b border-rule font-sans font-semibold text-ink">
            {title}
          </div>
        )}
        <div className="p-5">{children}</div>
      </div>
    </div>
  );
}
```

- [ ] **Step 7: tsc check**

```bash
cd apps/zeroship-builder && npx tsc --noEmit
```

Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add apps/zeroship-builder/src/client/components/{Spinner,EmptyState,ErrorState,Skeleton,Toast,Modal}.tsx
git commit -m "builder: add Spinner / EmptyState / ErrorState / Skeleton / Toast / Modal primitives"
```

---

## Task 6: TypeScript types for chat data parts

**Files:**
- Create: `apps/zeroship-builder/src/client/types/chat.ts`

Per design §8.2.7 (Survey contract) and §10.5 (Diff cards) and §11.1 (Critic dimensions).

- [ ] **Step 1: Create `types/chat.ts`**

```ts
// Type contract for the AI-SDK-protocol custom data parts.
// Server's stream translator emits these as `data-part` chunks; client renders them.

import { z } from "zod";

// --- Survey (per design §8.2.7) ---

export const optionSchema = z.object({
  value: z.string(),
  label: z.string(),
  hint: z.string().optional(),
});
export type Option = z.infer<typeof optionSchema>;

export const questionKindSchema = z.discriminatedUnion("type", [
  z.object({ type: z.literal("single_choice"), options: z.array(optionSchema) }),
  z.object({
    type: z.literal("multi_choice"),
    options: z.array(optionSchema),
    min: z.number().int().min(0).optional(),
    max: z.number().int().min(1).optional(),
  }),
  z.object({
    type: z.literal("short_text"),
    placeholder: z.string().optional(),
    max_length: z.number().int().min(1).optional(),
  }),
  z.object({
    type: z.literal("long_text"),
    placeholder: z.string().optional(),
    max_length: z.number().int().min(1).optional(),
  }),
  z.object({ type: z.literal("yes_no") }),
  z.object({
    type: z.literal("scale"),
    min: z.number().int(),
    max: z.number().int(),
    labels: z.tuple([z.string(), z.string()]).optional(),
  }),
  z.object({
    type: z.literal("image_upload"),
    max_count: z.number().int().min(1).optional(),
    hint: z.string().optional(),
  }),
]);
export type QuestionKind = z.infer<typeof questionKindSchema>;

export const questionSchema = z.object({
  id: z.string().min(1),
  prompt: z.string().min(1),
  kind: questionKindSchema,
  default: z.unknown().optional(),
  required: z.boolean().optional(),
});
export type Question = z.infer<typeof questionSchema>;

export const surveySchema = z.object({
  preamble: z.string().optional(),
  questions: z.array(questionSchema).max(3),
  skip_label: z.string().optional(),
});
export type Survey = z.infer<typeof surveySchema>;

export type SurveyResponse = {
  survey_id: string;
  answers: Record<string, unknown>;
  skipped: boolean;
};

// --- Diff card ---

export const diffSchema = z.object({
  path: z.string(),
  before: z.string(),  // file content before
  after: z.string(),   // file content after
});
export type Diff = z.infer<typeof diffSchema>;

// --- Critic round indicator ---

export const criticRoundSchema = z.object({
  round: z.number().int().min(1),
  total: z.number().int().min(1),
  approved: z.boolean(),
  issues: z.array(z.object({
    dimension: z.string(),
    severity: z.enum(["low", "medium", "high", "critical"]),
    note: z.string(),
  })).default([]),
});
export type CriticRound = z.infer<typeof criticRoundSchema>;

// --- Custom data part union (what the translator emits) ---

export type CustomDataPart =
  | { kind: "survey";        payload: Survey }
  | { kind: "diff";          payload: Diff }
  | { kind: "critic-round";  payload: CriticRound };
```

- [ ] **Step 2: Verify tsc**

```bash
cd apps/zeroship-builder && npx tsc --noEmit
```

Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add apps/zeroship-builder/src/client/types/chat.ts
git commit -m "builder: add chat data-part type contract (Survey, Diff, CriticRound)"
```

---

## Task 7: TopBar component

**Files:**
- Create: `apps/zeroship-builder/src/client/workspace/TopBar.tsx`

- [ ] **Step 1: Create `workspace/TopBar.tsx`**

```tsx
import type { ReactNode } from "react";
import { Link } from "react-router-dom";

export interface TopBarProps {
  projectName?: string | null;
  /** Visible only when in a workspace; clicking jumps back home. */
  homeHref?: string;
  /** Center slot — usually a status strip (deploying, etc.). */
  center?: ReactNode;
  /** Right-side custom content rendered before the account dot. */
  right?: ReactNode;
  /** Account avatar / initials. Click → /account. */
  accountInitials?: string;
}

export function TopBar({
  projectName,
  homeHref = "/",
  center,
  right,
  accountInitials = "·",
}: TopBarProps) {
  return (
    <header
      data-testid="topbar"
      className="grid items-center gap-4 border-b border-rule bg-paper px-6 h-12"
      style={{ gridTemplateColumns: "minmax(280px, auto) 1fr auto" }}
    >
      <div className="flex items-baseline gap-3 min-w-0">
        <Link
          to={homeHref}
          data-testid="topbar-logo"
          className="font-display italic text-lg font-medium text-ink hover:opacity-80"
        >
          zeroship<span className="text-tomato">.</span>
        </Link>
        {projectName && (
          <>
            <span className="text-rule">/</span>
            <span
              data-testid="topbar-project"
              className="font-sans text-sm font-medium text-ink truncate"
            >
              {projectName}
            </span>
          </>
        )}
      </div>

      <div className="flex items-center justify-center min-w-0">{center}</div>

      <div className="flex items-center gap-3">
        {right}
        <Link
          to="/account"
          data-testid="topbar-account"
          aria-label="Account"
          className="inline-flex h-7 w-7 items-center justify-center rounded-full bg-ink text-paper font-sans text-xs font-semibold leading-none"
        >
          {accountInitials}
        </Link>
      </div>
    </header>
  );
}
```

- [ ] **Step 2: tsc check**

```bash
cd apps/zeroship-builder && npx tsc --noEmit
```

Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add apps/zeroship-builder/src/client/workspace/TopBar.tsx
git commit -m "builder: add TopBar component (workspace shell)"
```

---

## Task 8: CanvasPills + PreviewCanvasStub

**Files:**
- Create: `apps/zeroship-builder/src/client/workspace/CanvasPills.tsx`
- Create: `apps/zeroship-builder/src/client/workspace/PreviewCanvasStub.tsx`

- [ ] **Step 1: `CanvasPills.tsx`**

```tsx
import { Pill } from "../components/Pill";

const ALL_PILLS = [
  "preview",
  "logs",
  "plan",
  "health",
  "settings",
  // +Data tier (hidden in maker, revealed by toggle in later plans):
  // "data", "media",
  // +Code tier:
  // "files", "env",
] as const;

export type CanvasPillId = (typeof ALL_PILLS)[number];

export interface CanvasPillsProps {
  active: CanvasPillId;
  onChange: (id: CanvasPillId) => void;
  /** Visible pill set (filtered by tier — Plan 02+ wires this). */
  visible?: readonly CanvasPillId[];
}

export function CanvasPills({ active, onChange, visible = ALL_PILLS }: CanvasPillsProps) {
  return (
    <div data-testid="canvas-pills" className="flex items-center gap-1.5">
      {visible.map((id) => (
        <Pill
          key={id}
          size="sm"
          active={active === id}
          onClick={() => onChange(id)}
          data-testid={`pill:${id}`}
        >
          {id}
        </Pill>
      ))}
    </div>
  );
}
```

- [ ] **Step 2: `PreviewCanvasStub.tsx`**

```tsx
import { EmptyState } from "../components/EmptyState";

/** Plan 01 placeholder — real preview canvas in Plan 04. */
export function PreviewCanvasStub() {
  return (
    <div data-testid="preview-canvas" className="h-full flex items-center justify-center bg-paper-2">
      <EmptyState
        title="Nothing's been built yet."
        description="Tell the agent what to make in the chat on the right."
      />
    </div>
  );
}
```

- [ ] **Step 3: tsc check + commit**

```bash
cd apps/zeroship-builder && npx tsc --noEmit
git add apps/zeroship-builder/src/client/workspace/{CanvasPills,PreviewCanvasStub}.tsx
git commit -m "builder: add CanvasPills + PreviewCanvasStub"
```

---

## Task 9: Chat composer

**Files:**
- Create: `apps/zeroship-builder/src/client/workspace/chat/ChatComposer.tsx`

- [ ] **Step 1: Create `chat/ChatComposer.tsx`**

```tsx
import {
  useRef,
  useState,
  type ChangeEvent,
  type FormEvent,
  type KeyboardEvent,
} from "react";
import { Button } from "../../components/Button";
import { cn } from "../../lib/utils";

export interface ChatComposerProps {
  value: string;
  onChange: (value: string) => void;
  onSubmit: (text: string, attachments: File[]) => void;
  onStop?: () => void;
  busy: boolean;
  disabled?: boolean;
  placeholder?: string;
}

export function ChatComposer({
  value,
  onChange,
  onSubmit,
  onStop,
  busy,
  disabled,
  placeholder = "describe a change…",
}: ChatComposerProps) {
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [attachments, setAttachments] = useState<File[]>([]);

  function submit(e?: FormEvent) {
    e?.preventDefault();
    if (busy || disabled) return;
    const text = value.trim();
    if (!text && attachments.length === 0) return;
    onSubmit(text, attachments);
    onChange("");
    setAttachments([]);
  }

  function onKey(e: KeyboardEvent<HTMLTextAreaElement>) {
    // ⌘/Ctrl+Enter sends; plain Enter is newline.
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      submit();
    }
  }

  function onPickFiles(e: ChangeEvent<HTMLInputElement>) {
    const files = e.target.files;
    if (!files) return;
    setAttachments((prev) => [...prev, ...Array.from(files)]);
    if (fileInputRef.current) fileInputRef.current.value = "";
  }

  function removeAttachment(idx: number) {
    setAttachments((prev) => prev.filter((_, i) => i !== idx));
  }

  return (
    <form
      onSubmit={submit}
      data-testid="chat-composer"
      className={cn(
        "border-t border-rule bg-paper",
        "px-4 pt-3 pb-3",
      )}
    >
      {attachments.length > 0 && (
        <div className="flex flex-wrap gap-2 mb-2">
          {attachments.map((f, i) => (
            <div
              key={i}
              className="inline-flex items-center gap-1 px-2 py-1 bg-paper-2 border border-rule rounded text-[11px]"
            >
              <span className="font-mono">{f.name}</span>
              <button
                type="button"
                aria-label={`Remove ${f.name}`}
                onClick={() => removeAttachment(i)}
                className="text-ink-soft hover:text-blood cursor-pointer"
              >
                ✕
              </button>
            </div>
          ))}
        </div>
      )}

      <textarea
        rows={2}
        disabled={busy || disabled}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={onKey}
        placeholder={busy ? "thinking…" : placeholder}
        data-testid="chat-input"
        className={cn(
          "w-full resize-none bg-paper-2 border border-rule rounded",
          "px-3 py-2.5 font-serif text-[14.5px] leading-snug text-ink",
          "placeholder:text-pencil placeholder:italic",
          "focus:outline-none focus:border-ink",
          "disabled:opacity-50",
        )}
      />

      <div className="flex items-center justify-between mt-2">
        <div className="flex items-center gap-2 text-[10.5px] text-pencil font-sans uppercase tracking-wider">
          <span>⌘ ↵ to send · ↵ for newline</span>
        </div>
        <div className="flex items-center gap-2">
          <input
            ref={fileInputRef}
            type="file"
            multiple
            accept="image/*,.pdf,.json,.txt,.md"
            onChange={onPickFiles}
            className="hidden"
          />
          <Button
            type="button"
            variant="ghost"
            size="sm"
            onClick={() => fileInputRef.current?.click()}
            aria-label="Attach files"
          >
            📎
          </Button>
          {busy ? (
            <Button
              type="button"
              variant="destructive"
              size="sm"
              data-testid="chat-stop"
              onClick={onStop}
            >
              ■ Stop
            </Button>
          ) : (
            <Button
              type="submit"
              variant="primary"
              size="sm"
              data-testid="chat-send"
              disabled={!value.trim() && attachments.length === 0}
            >
              Send →
            </Button>
          )}
        </div>
      </div>
    </form>
  );
}
```

- [ ] **Step 2: tsc check**

```bash
cd apps/zeroship-builder && npx tsc --noEmit
```

Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add apps/zeroship-builder/src/client/workspace/chat/ChatComposer.tsx
git commit -m "builder: add ChatComposer (textarea, attachments, ⌘+↵, stop)"
```

---

## Task 10: Receipt + DiffCard + SurveyCard + CriticRoundCard (placeholder renderers)

**Files:**
- Create: `apps/zeroship-builder/src/client/workspace/chat/Receipt.tsx`
- Create: `apps/zeroship-builder/src/client/workspace/chat/DiffCard.tsx`
- Create: `apps/zeroship-builder/src/client/workspace/chat/SurveyCard.tsx`
- Create: `apps/zeroship-builder/src/client/workspace/chat/CriticRoundCard.tsx`

These render the data parts. Real fidelity grows in later plans; Plan 01 ships sufficient versions.

- [ ] **Step 1: `Receipt.tsx`**

```tsx
import { useState } from "react";
import { Spinner } from "../../components/Spinner";

export interface ReceiptProps {
  toolName: string;
  status: "running" | "done" | "error";
  summary?: string;
  inputJson?: unknown;
  outputJson?: unknown;
}

export function Receipt({ toolName, status, summary, inputJson, outputJson }: ReceiptProps) {
  const [open, setOpen] = useState(false);
  const showDetails = inputJson !== undefined || outputJson !== undefined;

  return (
    <div
      data-testid="receipt"
      data-status={status}
      className="mt-2 bg-paper border border-rule-2 rounded px-3 py-2.5"
    >
      <div className="flex items-start gap-3">
        <span aria-hidden="true" className="flex h-[18px] w-[18px] flex-shrink-0 items-center justify-center mt-0.5">
          {status === "done" && (
            <span className="inline-block size-[14px] rounded-full bg-ivy text-paper text-[10px] font-semibold leading-none flex items-center justify-center">
              ✓
            </span>
          )}
          {status === "error" && (
            <span className="inline-block size-[14px] rounded-full bg-blood text-paper text-[10px] font-semibold leading-none flex items-center justify-center">
              ✕
            </span>
          )}
          {status === "running" && <Spinner size={14} />}
        </span>
        <div className="flex-1 min-w-0 font-serif text-[13.5px] leading-snug text-ink">
          {summary ?? toolName}
          <div className="font-mono text-[10px] text-pencil mt-0.5">{toolName}</div>
        </div>
        {showDetails && (
          <button
            type="button"
            onClick={() => setOpen((v) => !v)}
            className="self-center font-sans text-[11px] text-pencil hover:text-ink cursor-pointer"
          >
            {open ? "hide" : "details"}
          </button>
        )}
      </div>
      {open && (
        <div className="mt-2 pt-2 border-t border-rule-2 space-y-2">
          {inputJson !== undefined && (
            <div>
              <div className="font-sans text-[10px] uppercase tracking-wider text-pencil mb-0.5">input</div>
              <pre className="font-mono text-[10.5px] whitespace-pre-wrap break-all max-h-32 overflow-auto bg-paper-2 px-2 py-1 border border-rule-2 rounded">
                {JSON.stringify(inputJson, null, 2)}
              </pre>
            </div>
          )}
          {outputJson !== undefined && (
            <div>
              <div className="font-sans text-[10px] uppercase tracking-wider text-pencil mb-0.5">output</div>
              <pre className="font-mono text-[10.5px] whitespace-pre-wrap break-all max-h-32 overflow-auto bg-paper-2 px-2 py-1 border border-rule-2 rounded">
                {typeof outputJson === "string" ? outputJson : JSON.stringify(outputJson, null, 2)}
              </pre>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
```

- [ ] **Step 2: `DiffCard.tsx` (placeholder — full Monaco diff in Plan 04)**

```tsx
import type { Diff } from "../../types/chat";

export function DiffCard({ diff }: { diff: Diff }) {
  return (
    <div data-testid="diff-card" className="mt-2 bg-paper border border-rule-2 rounded">
      <div className="px-3 py-1.5 border-b border-rule-2 font-mono text-[11px] text-ink-soft flex items-center justify-between">
        <span>{diff.path}</span>
        <span className="text-pencil text-[10px]">diff</span>
      </div>
      <div className="p-3 font-mono text-[11px] leading-snug whitespace-pre overflow-x-auto max-h-48">
        {/* simple line-diff for Plan 01; LSP-grade diff in Plan 04 */}
        {simpleDiff(diff.before, diff.after).map((line, i) => (
          <div key={i} className={
            line.kind === "add"    ? "bg-ivy/10 text-ivy" :
            line.kind === "remove" ? "bg-blood/10 text-blood" :
            "text-ink-soft"
          }>
            {line.kind === "add" ? "+ " : line.kind === "remove" ? "- " : "  "}
            {line.text}
          </div>
        ))}
      </div>
    </div>
  );
}

interface DiffLine { kind: "add" | "remove" | "context"; text: string; }

function simpleDiff(before: string, after: string): DiffLine[] {
  const a = before.split("\n");
  const b = after.split("\n");
  const out: DiffLine[] = [];
  let i = 0, j = 0;
  while (i < a.length || j < b.length) {
    if (i < a.length && j < b.length && a[i] === b[j]) {
      out.push({ kind: "context", text: a[i] });
      i++; j++;
    } else if (j < b.length && (i >= a.length || a[i] !== b[j])) {
      out.push({ kind: "add", text: b[j] });
      j++;
    } else {
      out.push({ kind: "remove", text: a[i] });
      i++;
    }
  }
  return out;
}
```

- [ ] **Step 3: `SurveyCard.tsx` (functional — single_choice + skip — full kinds in Plan 02)**

```tsx
import { useState } from "react";
import { Button } from "../../components/Button";
import type { Survey, SurveyResponse } from "../../types/chat";

export interface SurveyCardProps {
  survey: Survey;
  onSubmit: (response: SurveyResponse) => void;
  onSkip?: () => void;
  surveyId?: string;
}

export function SurveyCard({ survey, onSubmit, onSkip, surveyId = "anon" }: SurveyCardProps) {
  const [answers, setAnswers] = useState<Record<string, unknown>>({});
  const [submitted, setSubmitted] = useState(false);

  // Defensive: truncate to 3 questions per spec.
  const questions = survey.questions.slice(0, 3);

  function pick(qId: string, value: unknown) {
    setAnswers((a) => ({ ...a, [qId]: value }));
  }

  function submit() {
    const merged: Record<string, unknown> = {};
    for (const q of questions) {
      merged[q.id] = answers[q.id] ?? q.default;
    }
    setSubmitted(true);
    onSubmit({ survey_id: surveyId, answers: merged, skipped: false });
  }

  function skip() {
    setSubmitted(true);
    onSkip?.();
  }

  if (submitted) {
    return (
      <div data-testid="survey-card-collapsed" className="mt-2 px-3 py-1.5 border border-rule-2 bg-paper-2 rounded text-[12px] text-ink-soft">
        Answered.
      </div>
    );
  }

  const allRequiredAnswered = questions
    .filter((q) => q.required)
    .every((q) => answers[q.id] !== undefined);

  return (
    <div data-testid="survey-card" className="mt-2 bg-paper border border-rule rounded p-3">
      {survey.preamble && (
        <p className="font-serif text-[13.5px] text-ink mb-3">{survey.preamble}</p>
      )}
      <div className="space-y-3">
        {questions.map((q) => (
          <div key={q.id}>
            <div className="font-sans text-[12px] font-medium text-ink mb-1.5">{q.prompt}</div>
            {q.kind.type === "single_choice" && (
              <div className="flex flex-wrap gap-2">
                {q.kind.options.slice(0, 6).map((opt) => (
                  <Button
                    key={opt.value}
                    type="button"
                    size="sm"
                    variant={answers[q.id] === opt.value ? "primary" : "secondary"}
                    onClick={() => pick(q.id, opt.value)}
                  >
                    {opt.label}
                  </Button>
                ))}
              </div>
            )}
            {q.kind.type === "yes_no" && (
              <div className="flex gap-2">
                <Button
                  type="button"
                  size="sm"
                  variant={answers[q.id] === true ? "primary" : "secondary"}
                  onClick={() => pick(q.id, true)}
                >
                  Yes
                </Button>
                <Button
                  type="button"
                  size="sm"
                  variant={answers[q.id] === false ? "primary" : "secondary"}
                  onClick={() => pick(q.id, false)}
                >
                  No
                </Button>
              </div>
            )}
            {q.kind.type === "short_text" && (
              <input
                type="text"
                placeholder={q.kind.placeholder}
                maxLength={q.kind.max_length}
                value={(answers[q.id] as string | undefined) ?? ""}
                onChange={(e) => pick(q.id, e.target.value)}
                className="w-full px-2 py-1.5 border border-rule bg-paper-2 rounded font-sans text-[13px] focus:outline-none focus:border-ink"
              />
            )}
            {q.kind.type === "long_text" && (
              <textarea
                rows={2}
                placeholder={q.kind.placeholder}
                maxLength={q.kind.max_length}
                value={(answers[q.id] as string | undefined) ?? ""}
                onChange={(e) => pick(q.id, e.target.value)}
                className="w-full px-2 py-1.5 border border-rule bg-paper-2 rounded font-sans text-[13px] focus:outline-none focus:border-ink resize-none"
              />
            )}
            {/* multi_choice / scale / image_upload — covered in Plan 02 */}
          </div>
        ))}
      </div>
      <div className="flex justify-between items-center mt-3">
        {onSkip ? (
          <button
            type="button"
            onClick={skip}
            className="font-sans text-[11px] text-pencil hover:text-ink cursor-pointer"
          >
            {survey.skip_label ?? "skip — just build"}
          </button>
        ) : <span />}
        <Button type="button" size="sm" variant="primary" onClick={submit} disabled={!allRequiredAnswered}>
          Send →
        </Button>
      </div>
    </div>
  );
}
```

- [ ] **Step 4: `CriticRoundCard.tsx` (small inline indicator)**

```tsx
import type { CriticRound } from "../../types/chat";

export function CriticRoundCard({ round }: { round: CriticRound }) {
  return (
    <div
      data-testid="critic-round-card"
      className="mt-1.5 inline-flex items-center gap-2 px-2 py-1 bg-paper-2 border border-rule-2 rounded"
    >
      <span className="font-sans text-[10px] uppercase tracking-wider text-pencil">
        critic round {round.round}/{round.total}
      </span>
      <span className={
        "font-sans text-[10px] " +
        (round.approved ? "text-ivy" : "text-amber")
      }>
        {round.approved ? "approved" : `${round.issues.length} concerns`}
      </span>
    </div>
  );
}
```

- [ ] **Step 5: tsc check + commit**

```bash
cd apps/zeroship-builder && npx tsc --noEmit
git add apps/zeroship-builder/src/client/workspace/chat/{Receipt,DiffCard,SurveyCard,CriticRoundCard}.tsx
git commit -m "builder: add Receipt / DiffCard / SurveyCard / CriticRoundCard"
```

---

## Task 11: Message renderers (user + assistant)

**Files:**
- Create: `apps/zeroship-builder/src/client/workspace/chat/MessageUser.tsx`
- Create: `apps/zeroship-builder/src/client/workspace/chat/MessageAssistant.tsx`

- [ ] **Step 1: `MessageUser.tsx`**

```tsx
import type { ReactNode } from "react";

export interface MessageUserProps {
  text: string;
  time?: string;
  attachments?: ReactNode;
}

export function MessageUser({ text, time, attachments }: MessageUserProps) {
  return (
    <div data-testid="msg-user">
      <div className="font-sans text-[10px] uppercase tracking-wider text-pencil mb-1">
        You{time ? ` · ${time}` : ""}
      </div>
      <div className="font-serif text-[15px] leading-snug text-ink border-l-2 border-tomato pl-3.5 whitespace-pre-wrap break-words">
        {text}
      </div>
      {attachments && <div className="mt-1 pl-3.5">{attachments}</div>}
    </div>
  );
}
```

- [ ] **Step 2: `MessageAssistant.tsx`**

This is the more complex one. It renders a streaming-aware message body plus a slot for tool/data parts.

```tsx
import type { ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

export interface MessageAssistantProps {
  text: string;
  streaming?: boolean;
  /** Pre-rendered children: receipts, diffs, surveys, critic-rounds. */
  parts?: ReactNode;
}

export function MessageAssistant({ text, streaming, parts }: MessageAssistantProps) {
  return (
    <div data-testid="msg-assistant">
      <div className="font-sans text-[10px] uppercase tracking-wider text-ink-soft mb-1">
        Builder
      </div>
      <div className="font-serif text-[14.5px] leading-snug text-ink prose-headings:font-display prose-code:font-mono prose-code:text-[13px]">
        {text ? (
          <ReactMarkdown remarkPlugins={[remarkGfm]}>{text}</ReactMarkdown>
        ) : streaming ? (
          <span className="italic text-pencil">thinking…</span>
        ) : null}
        {streaming && text && (
          <span
            aria-hidden="true"
            className="inline-block ml-0.5 w-[6px] h-[14px] bg-ink/70 align-middle"
          />
        )}
      </div>
      {parts && <div className="mt-1">{parts}</div>}
    </div>
  );
}
```

- [ ] **Step 3: tsc check + commit**

```bash
cd apps/zeroship-builder && npx tsc --noEmit
git add apps/zeroship-builder/src/client/workspace/chat/{MessageUser,MessageAssistant}.tsx
git commit -m "builder: add MessageUser / MessageAssistant renderers"
```

---

## Task 12: Server stream helpers

**Files:**
- Create: `apps/zeroship-builder/src/server/_shared/stream.ts`

These helpers emit AI-SDK-compatible stream chunks. The mock `chat.ts` uses them in Task 13. The translator (Plan 02) reuses them.

- [ ] **Step 1: Create `_shared/stream.ts`**

```ts
"use server";
// AI SDK stream protocol helpers.
// AI SDK uses Server-Sent Events with each chunk being one of these types.
// Reference: https://sdk.vercel.ai/docs/ai-sdk-ui/stream-protocol

export type AIStreamChunk =
  | { type: "text-delta"; delta: string }
  | { type: "tool-call"; toolCallId: string; toolName: string; args: unknown }
  | { type: "tool-result"; toolCallId: string; result: unknown }
  | { type: "data-part"; partName: string; payload: unknown }
  | { type: "error"; message: string }
  | { type: "finish"; usage?: { inputTokens?: number; outputTokens?: number } };

/** Encode a single chunk as the AI SDK SSE wire format. */
export function encodeChunk(chunk: AIStreamChunk): string {
  // The wire format the @ai-sdk/react useChat hook understands is one of
  // several "stream protocols". We use the data-stream protocol for typed
  // chunks. Each line is `<type-prefix>:<payload-json>\n` for the legacy
  // format, OR newline-delimited JSON for the newer protocol used in AI SDK 4.
  // For Plan 01 we ship the newline-delimited JSON form which @ai-sdk/react
  // accepts when the response Content-Type is "text/plain" with proper headers.
  return JSON.stringify(chunk) + "\n";
}

/** Build a Response that streams from an async iterable of chunks. */
export function streamResponse(
  source: AsyncIterable<AIStreamChunk>,
): Response {
  const encoder = new TextEncoder();
  const stream = new ReadableStream<Uint8Array>({
    async start(controller) {
      try {
        for await (const chunk of source) {
          controller.enqueue(encoder.encode(encodeChunk(chunk)));
        }
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        controller.enqueue(encoder.encode(encodeChunk({ type: "error", message })));
      } finally {
        controller.enqueue(encoder.encode(encodeChunk({ type: "finish" })));
        controller.close();
      }
    },
  });
  return new Response(stream, {
    headers: {
      "Content-Type": "text/plain; charset=utf-8",
      "Cache-Control": "no-cache, no-transform",
      "X-Accel-Buffering": "no",
    },
  });
}

/** Convenience: a delay that yields control. Used by the mock to feel realistic. */
export function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
```

> **Note for the engineer:** the AI SDK supports multiple stream protocols. `@ai-sdk/react`'s `useChat` hook supports both the data-stream protocol (line-delimited JSON) and the SSE-based stream protocol. For Plan 01 we pick line-delimited JSON because it's simpler to emit and `useChat` accepts it via `streamProtocol: "data"` config (Task 14).

- [ ] **Step 2: tsc check + commit**

```bash
cd apps/zeroship-builder && npx tsc --noEmit
git add apps/zeroship-builder/src/server/_shared/stream.ts
git commit -m "builder: add server stream helpers (encode + streamResponse)"
```

---

## Task 13: Mock chat server function

**Files:**
- Create: `apps/zeroship-builder/src/server/chat.ts`

A "use server" module that emits a realistic-looking stream. Plan 02 replaces the body with the real Builder loop; the wire shape stays the same.

- [ ] **Step 1: Create `server/chat.ts`**

```ts
"use server";
import {
  type AIStreamChunk,
  delay,
  streamResponse,
} from "./_shared/stream";

export interface ChatTurnInput {
  /** Plain text prompt. */
  text: string;
  /** Image attachments — V1 supports image input only; later expands. */
  images?: Array<{ name: string; mediaType: string; bytes: Uint8Array }>;
}

/**
 * Mock chat — produces a streamed sequence that exercises every
 * data-part shape the client handles.
 *
 * Plan 02 replaces the body with the deepagents → translator pipeline.
 * The wire format (AIStreamChunk) stays the same.
 */
export async function postChat(input: ChatTurnInput): Promise<Response> {
  return streamResponse(generate(input));
}

async function* generate(input: ChatTurnInput): AsyncIterable<AIStreamChunk> {
  // 1. Initial preamble text streaming
  const preamble = "Got it — let me think about that.\n\n";
  for (const ch of preamble) {
    yield { type: "text-delta", delta: ch };
    await delay(15);
  }

  // 2. A survey data part (asks one clarifying question)
  yield {
    type: "data-part",
    partName: "survey",
    payload: {
      preamble: "A quick thing first:",
      questions: [
        {
          id: "vibe",
          prompt: "Vibe?",
          kind: {
            type: "single_choice",
            options: [
              { value: "cozy",     label: "cozy / warm" },
              { value: "minimal",  label: "minimal" },
              { value: "playful",  label: "playful" },
            ],
          },
          default: "minimal",
        },
      ],
      skip_label: "skip — just build",
    },
  };

  // For Plan 01 the mock proceeds whether or not the user answers
  // (we don't yet wait on a real response from the client).
  await delay(800);

  // 3. Resume text
  const resume = "\nOK — I'll start by writing a small file.\n\n";
  for (const ch of resume) {
    yield { type: "text-delta", delta: ch };
    await delay(12);
  }

  // 4. A tool call (write_file)
  const toolCallId = "tool_" + Math.random().toString(36).slice(2, 10);
  yield {
    type: "tool-call",
    toolCallId,
    toolName: "write_file",
    args: { path: "src/index.tsx", contents_preview: "<… mock contents …>" },
  };
  await delay(600);

  yield {
    type: "tool-result",
    toolCallId,
    result: { ok: true, bytes_written: 312 },
  };

  // 5. A diff card
  yield {
    type: "data-part",
    partName: "diff",
    payload: {
      path: "src/index.tsx",
      before: "",
      after:
        "import { render } from 'react-dom';\n" +
        "render(<h1>Hello</h1>, document.body);\n",
    },
  };

  // 6. A critic round
  yield {
    type: "data-part",
    partName: "critic-round",
    payload: { round: 1, total: 3, approved: true, issues: [] },
  };

  // 7. Final text
  const trailer = "\nDone. (This is the Plan 01 mock — Plan 02 wires the real Builder agent.)\n";
  for (const ch of trailer) {
    yield { type: "text-delta", delta: ch };
    await delay(10);
  }
}
```

- [ ] **Step 2: tsc check + commit**

```bash
cd apps/zeroship-builder && npx tsc --noEmit
git add apps/zeroship-builder/src/server/chat.ts
git commit -m "builder: add mock chat server fn (Plan 01 placeholder)"
```

---

## Task 14: ChatRail container with `useChat` integration

**Files:**
- Create: `apps/zeroship-builder/src/client/workspace/chat/ChatRail.tsx`
- Create: `apps/zeroship-builder/src/client/workspace/chat/ChatMessages.tsx`

This is where the AI SDK client meets our renderers. The ChatRail owns the `useChat` state; ChatMessages renders.

- [ ] **Step 1: Add the AI-SDK config**

Important: the AI SDK's `useChat` hook expects either:
- A REST endpoint URL (`api: "/api/chat"`)
- Or a custom fetch handler

Our chat lives behind a `"use server"` server function (`postChat`), exposed by the vite-plugin as a chunked RPC at a known URL. We set `useChat`'s `api` to the URL the plugin generates for `postChat`.

The vite-plugin emits server functions at `/_zs/server/<modulePath>/<exportName>`. So the chat URL is `/_zs/server/server/chat.ts/postChat` (verified by reading the plugin source if needed).

For Plan 01 we hard-code the URL. Plan 02 introduces a generated client.

- [ ] **Step 2: Create `chat/ChatMessages.tsx`**

```tsx
import { useEffect, useRef } from "react";
import type { UIMessage } from "ai";
import { MessageUser } from "./MessageUser";
import { MessageAssistant } from "./MessageAssistant";
import { Receipt } from "./Receipt";
import { DiffCard } from "./DiffCard";
import { SurveyCard } from "./SurveyCard";
import { CriticRoundCard } from "./CriticRoundCard";
import type { Diff, Survey, CriticRound, SurveyResponse } from "../../types/chat";

export interface ChatMessagesProps {
  messages: UIMessage[];
  busy: boolean;
  onSurveySubmit?: (response: SurveyResponse) => void;
}

export function ChatMessages({ messages, busy, onSurveySubmit }: ChatMessagesProps) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);

  useEffect(() => {
    if (!stickToBottom.current) return;
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [messages]);

  function onScroll() {
    const el = scrollRef.current;
    if (!el) return;
    const dist = el.scrollHeight - el.clientHeight - el.scrollTop;
    stickToBottom.current = dist < 60;
  }

  return (
    <div
      ref={scrollRef}
      onScroll={onScroll}
      data-testid="chat-messages"
      className="flex-1 overflow-y-auto px-5 py-4 flex flex-col gap-5 min-h-0"
    >
      {messages.length === 0 && !busy && (
        <div className="font-serif text-[14px] italic text-ink-soft">
          Tell Builder what to make.
        </div>
      )}

      {messages.map((m, idx) => {
        if (m.role === "user") {
          // Concatenate any text parts into a single user-text body.
          const text = m.parts
            .filter((p): p is Extract<typeof p, { type: "text" }> => p.type === "text")
            .map((p) => p.text)
            .join("");
          return <MessageUser key={m.id} text={text} />;
        }

        if (m.role === "assistant") {
          const isLast = idx === messages.length - 1;
          let text = "";
          const renderedParts: React.ReactNode[] = [];
          for (const p of m.parts) {
            if (p.type === "text") {
              text += p.text;
            } else if (p.type === "tool-call" || p.type === "tool-result") {
              // Combined into a Receipt below by toolCallId
            } else if (p.type === "data-part") {
              const dp = p as { type: "data-part"; partName: string; payload: unknown };
              if (dp.partName === "diff") {
                renderedParts.push(<DiffCard key={`${m.id}-${renderedParts.length}`} diff={dp.payload as Diff} />);
              } else if (dp.partName === "survey") {
                renderedParts.push(
                  <SurveyCard
                    key={`${m.id}-${renderedParts.length}`}
                    surveyId={`${m.id}-survey-${renderedParts.length}`}
                    survey={dp.payload as Survey}
                    onSubmit={(r) => onSurveySubmit?.(r)}
                    onSkip={() => onSurveySubmit?.({
                      survey_id: `${m.id}`,
                      answers: {},
                      skipped: true,
                    })}
                  />,
                );
              } else if (dp.partName === "critic-round") {
                renderedParts.push(<CriticRoundCard key={`${m.id}-${renderedParts.length}`} round={dp.payload as CriticRound} />);
              }
            }
          }

          // Group tool-call / tool-result pairs into Receipts.
          const calls = m.parts.filter((p): p is Extract<typeof p, { type: "tool-call" }> => p.type === "tool-call");
          const results = m.parts.filter((p): p is Extract<typeof p, { type: "tool-result" }> => p.type === "tool-result");
          const resultById = new Map(results.map((r) => [r.toolCallId, r] as const));
          for (const c of calls) {
            const res = resultById.get(c.toolCallId);
            renderedParts.unshift(
              <Receipt
                key={c.toolCallId}
                toolName={c.toolName}
                status={res ? "done" : "running"}
                summary={summarizeTool(c.toolName, c.args)}
                inputJson={c.args}
                outputJson={res?.result}
              />,
            );
          }

          return (
            <MessageAssistant
              key={m.id}
              text={text}
              streaming={isLast && busy}
              parts={renderedParts.length > 0 ? <>{renderedParts}</> : null}
            />
          );
        }

        return null;
      })}
    </div>
  );
}

function summarizeTool(name: string, args: unknown): string {
  // Minimal Plan 01 humanization. Plan 02 expands.
  if (name === "write_file") {
    const path = (args as { path?: string } | undefined)?.path;
    return path ? `Wrote \`${path}\`.` : "Wrote a file.";
  }
  if (name === "ask_survey") return "Asked a clarifying question.";
  return `Ran ${name}.`;
}
```

- [ ] **Step 3: Create `chat/ChatRail.tsx`**

```tsx
import { useState } from "react";
import { useChat } from "@ai-sdk/react";
import type { UIMessage } from "ai";
import { ChatComposer } from "./ChatComposer";
import { ChatMessages } from "./ChatMessages";
import type { SurveyResponse } from "../../types/chat";

const CHAT_API_URL = "/_zs/server/server/chat.ts/postChat";

export interface ChatRailProps {
  appName?: string;
}

export function ChatRail({ appName }: ChatRailProps) {
  const [input, setInput] = useState("");

  const { messages, sendMessage, status, stop, error } = useChat({
    api: CHAT_API_URL,
    streamProtocol: "data",
  });

  const busy = status === "submitted" || status === "streaming";

  function handleSubmit(text: string, attachments: File[]) {
    void sendMessage(
      {
        role: "user",
        parts: [{ type: "text", text }],
      },
      {
        body: {
          // Server function expects { text, images? }. attachments → images.
          text,
          // Plan 01: ignore attachments to keep wire simple. Plan 02 wires images.
        },
      },
    );
  }

  function handleSurveySubmit(_response: SurveyResponse) {
    // Plan 02 will round-trip the response via a follow-up sendMessage.
    // For Plan 01 the mock doesn't wait — we just collapse the survey.
  }

  return (
    <div data-testid="chat-rail" className="flex flex-col h-full bg-paper-2">
      <div className="px-5 pt-4 pb-2 border-b border-rule flex items-baseline justify-between">
        <h3 className="font-display italic font-medium text-base">Notes &amp; thoughts</h3>
        <span className="font-sans text-[10px] uppercase tracking-wider text-pencil">
          {messages.length} {messages.length === 1 ? "turn" : "turns"}
        </span>
      </div>

      <ChatMessages
        messages={messages as UIMessage[]}
        busy={busy}
        onSurveySubmit={handleSurveySubmit}
      />

      {error && (
        <div className="px-5 py-2 border-t border-blood/30 bg-blood/5 font-sans text-[12px] text-blood">
          {error.message}
        </div>
      )}

      <ChatComposer
        value={input}
        onChange={setInput}
        onSubmit={handleSubmit}
        onStop={() => stop()}
        busy={busy}
        placeholder={appName ? `Tell ${appName} what to make.` : "Describe what to make."}
      />
    </div>
  );
}
```

- [ ] **Step 4: tsc check**

```bash
cd apps/zeroship-builder && npx tsc --noEmit
```

Expected: clean. If you see TS errors about `UIMessage` or `useChat`, check that `@ai-sdk/react` and `ai` are version `^1.0.0` and `^4.0.0` respectively (per Task 1) — earlier AI SDK versions used different types.

- [ ] **Step 5: Commit**

```bash
git add apps/zeroship-builder/src/client/workspace/chat/{ChatRail,ChatMessages}.tsx
git commit -m "builder: wire useChat hook to mock server (ChatRail + ChatMessages)"
```

---

## Task 15: Workspace shell layout

**Files:**
- Create: `apps/zeroship-builder/src/client/workspace/WorkspaceShell.tsx`

- [ ] **Step 1: Create `workspace/WorkspaceShell.tsx`**

```tsx
import { useState } from "react";
import { TopBar } from "./TopBar";
import { CanvasPills, type CanvasPillId } from "./CanvasPills";
import { PreviewCanvasStub } from "./PreviewCanvasStub";
import { ChatRail } from "./chat/ChatRail";

export interface WorkspaceShellProps {
  /** Plan 03 introduces real project routing; Plan 01 hardcodes "untitled". */
  projectName?: string;
}

export function WorkspaceShell({ projectName = "untitled" }: WorkspaceShellProps) {
  const [active, setActive] = useState<CanvasPillId>("preview");

  return (
    <div className="h-screen flex flex-col bg-paper">
      <TopBar
        projectName={projectName}
        center={
          <div className="flex items-center gap-3">
            <CanvasPills active={active} onChange={setActive} />
          </div>
        }
        right={
          <a
            href="#"
            data-testid="topbar-url"
            className="inline-flex items-center gap-2 px-3 py-1.5 border border-rule rounded-full bg-paper-2 font-mono text-[11px] text-ink-soft hover:border-ink hover:text-ink"
            style={{ textDecoration: "none" }}
          >
            <span className="size-[5px] rounded-full bg-ivy pulse-dot" aria-hidden="true" />
            {projectName}.zeroship.app
          </a>
        }
        accountInitials="ZS"
      />

      <div
        className="flex-1 grid min-h-0"
        style={{ gridTemplateColumns: "1fr 320px" }}
      >
        <main data-testid="canvas-area" className="min-h-0 min-w-0 overflow-hidden flex flex-col">
          {/* Plan 01 only renders preview; pill switching is wired but other
              canvases are placeholders. Plans 03–06 fill them in. */}
          {active === "preview" && <PreviewCanvasStub />}
          {active !== "preview" && (
            <div className="h-full flex items-center justify-center text-ink-soft font-serif italic">
              "{active}" canvas — coming in a later plan.
            </div>
          )}
        </main>
        <aside className="border-l border-rule min-h-0 min-w-0 overflow-hidden flex flex-col">
          <ChatRail appName={projectName} />
        </aside>
      </div>
    </div>
  );
}
```

- [ ] **Step 2: tsc check + commit**

```bash
cd apps/zeroship-builder && npx tsc --noEmit
git add apps/zeroship-builder/src/client/workspace/WorkspaceShell.tsx
git commit -m "builder: add WorkspaceShell layout"
```

---

## Task 16: Replace `App.tsx` with the minimal Plan 01 router

**Files:**
- Modify: `apps/zeroship-builder/src/client/App.tsx`

For Plan 01, the app is a single page: the workspace shell. Plan 03 introduces real routes (auth, gallery, etc.).

- [ ] **Step 1: Replace `App.tsx`**

```tsx
import { BrowserRouter, Routes, Route } from "react-router-dom";
import { WorkspaceShell } from "./workspace/WorkspaceShell";

export default function App() {
  return (
    <BrowserRouter>
      <Routes>
        <Route path="*" element={<WorkspaceShell />} />
      </Routes>
    </BrowserRouter>
  );
}
```

- [ ] **Step 2: Verify dev server**

```bash
cd apps/zeroship-builder && npm run dev &
sleep 3
# Browse http://localhost:5173 in browser. You should see:
# - The new TopBar with brand "zeroship." + "/ untitled" + URL pill on right.
# - Canvas area says "Nothing's been built yet."
# - Chat rail on right says "Notes & thoughts · 0 turns" + composer.
```

Expected: rendered shell. Type a prompt, hit ⌘+↵, watch streamed response with a SurveyCard, Receipt, DiffCard, CriticRoundCard appear in sequence.

If it doesn't work:
- Open devtools network tab. The request URL should be `/_zs/server/server/chat.ts/postChat`. If it's 404, check `vite.config.ts` and the `@zeroship/vite-plugin` is registered.
- Check browser console for AI SDK protocol errors.

- [ ] **Step 3: Kill dev server + commit**

```bash
# Kill the dev server: `kill %1` or ctrl+C
git add apps/zeroship-builder/src/client/App.tsx
git commit -m "builder: replace App.tsx with Plan 01 minimal shell"
```

---

## Task 17: Playwright E2E for the mock chat loop

**Files:**
- Create: `apps/zeroship-builder/e2e/chat-mock.spec.ts`

- [ ] **Step 1: Create `e2e/chat-mock.spec.ts`**

```ts
import { test, expect } from "@playwright/test";

test.describe("Plan 01 — workspace shell + mock chat", () => {
  test("renders the shell with project name and chat rail", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByTestId("topbar")).toBeVisible();
    await expect(page.getByTestId("topbar-project")).toContainText("untitled");
    await expect(page.getByTestId("topbar-url")).toContainText(".zeroship.app");
    await expect(page.getByTestId("chat-rail")).toBeVisible();
    await expect(page.getByTestId("preview-canvas")).toContainText("Nothing's been built");
  });

  test("submits a prompt and streams a response with all data parts", async ({ page }) => {
    await page.goto("/");

    const input = page.getByTestId("chat-input");
    await input.fill("Build a recipe app for my supper club");

    // ⌘+Enter (use Meta on Mac, Control on Linux/CI). Playwright handles both
    // via the modifier label.
    await input.press("Control+Enter");

    // User turn shows up immediately
    await expect(page.getByTestId("msg-user")).toContainText("recipe app for my supper club");

    // Assistant turn streams in
    const assistant = page.getByTestId("msg-assistant").last();
    await expect(assistant).toContainText("Got it", { timeout: 5000 });

    // SurveyCard appears
    await expect(page.getByTestId("survey-card")).toBeVisible({ timeout: 5000 });

    // Receipt appears (the write_file tool call)
    await expect(page.getByTestId("receipt").first()).toBeVisible({ timeout: 10000 });
    await expect(page.getByTestId("receipt").first()).toContainText("Wrote");

    // DiffCard appears
    await expect(page.getByTestId("diff-card")).toBeVisible({ timeout: 10000 });
    await expect(page.getByTestId("diff-card")).toContainText("src/index.tsx");

    // CriticRoundCard appears
    await expect(page.getByTestId("critic-round-card")).toBeVisible({ timeout: 10000 });
    await expect(page.getByTestId("critic-round-card")).toContainText("approved");
  });

  test("stop button cancels an in-flight stream", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("chat-input").fill("hello");
    await page.getByTestId("chat-input").press("Control+Enter");
    await expect(page.getByTestId("chat-stop")).toBeVisible({ timeout: 5000 });
    await page.getByTestId("chat-stop").click();
    await expect(page.getByTestId("chat-send")).toBeVisible({ timeout: 5000 });
  });

  test("survey 'skip' collapses the card", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("chat-input").fill("test");
    await page.getByTestId("chat-input").press("Control+Enter");
    await expect(page.getByTestId("survey-card")).toBeVisible({ timeout: 8000 });
    await page.getByText("skip — just build").click();
    await expect(page.getByTestId("survey-card-collapsed")).toBeVisible();
  });
});
```

- [ ] **Step 2: Verify Playwright config exists**

```bash
cat apps/zeroship-builder/playwright.config.ts
```

Expected: a config file. If it points to a different baseURL or webServer command, that's fine — adjust the test to match (or update the config). The defaults in the existing config should work.

- [ ] **Step 3: Run the e2e suite**

```bash
cd apps/zeroship-builder && npm run test:e2e
```

Expected: all 4 tests pass. If they don't:
- "topbar not visible" → dev server isn't running; check `playwright.config.ts`'s `webServer` block.
- "msg-assistant not found" → `useChat` isn't streaming; verify Network tab shows a 200 from `/_zs/server/server/chat.ts/postChat` and the body is line-delimited JSON.
- "survey-card not found" → check `data-part` chunks reach the client; AI SDK 4 emits them under `message.parts` with `type: "data-part"`. If your AI SDK version names this differently (e.g., older `data` callback), adapt `ChatMessages.tsx` accordingly.

- [ ] **Step 4: Commit**

```bash
git add apps/zeroship-builder/e2e/chat-mock.spec.ts
git commit -m "builder: add Plan 01 e2e (mock chat loop)"
```

---

## Task 18: Final verification

- [ ] **Step 1: Verify workspace builds clean**

```bash
cd apps/zeroship-builder && npm run build
```

Expected: build succeeds. Output: `dist/assets/...` for client, `dist/server/index.js` for server (vite-plugin combines `"use server"` modules). No TS errors. No build warnings about unresolved imports.

- [ ] **Step 2: Run e2e against the production build**

```bash
cd apps/zeroship-builder && npm run preview &
sleep 3
PLAYWRIGHT_BASE_URL=http://localhost:4173 npm run test:e2e
kill %1
```

Expected: all 4 tests pass against the production build too.

- [ ] **Step 3: Visual smoke check**

```bash
cd apps/zeroship-builder && npm run dev &
sleep 3
echo "Open http://localhost:5173 and confirm:"
echo "  · TopBar shows brand + 'untitled' crumb + URL pill"
echo "  · Canvas pills (preview/logs/plan/health/settings) are visible and switchable"
echo "  · Chat rail is on the right at 320px"
echo "  · Composer accepts text + ⌘+Enter sends + 📎 opens file picker"
echo "  · A streamed response appears with: text, SurveyCard, Receipt, DiffCard, CriticRoundCard"
echo "  · Stop button replaces Send while busy"
echo "  · Skipping the survey collapses it to 'Answered.'"
# kill dev server when done
```

- [ ] **Step 4: Sanity check — bundle size**

```bash
cd apps/zeroship-builder && npm run build
ls -lah dist/assets/*.js
```

Expected: client bundle ≤ ~300 KB gzipped (Plan 01 doesn't hit the §25 ≤ 200 KB target yet because we're not lazy-loading Monaco/CodeMirror; Plan 04 enforces). Note actual size in the commit message — it's a baseline for future regressions.

- [ ] **Step 5: Commit any final tweaks + tag**

```bash
git status              # should be clean
git tag plan-01-foundation
```

The tag is a checkpoint; future plans build forward from it.

---

## Self-review

Spec coverage:
- ✅ §1 foundation decisions — workspace shell exists; tier toggle is a placeholder (Plan 02+ wires it)
- ✅ §3.1 site map — workspace lives at `*` for now; full routing in Plan 03
- ✅ §3.3 workspace shell layout — TopBar + canvas + chat rail
- ✅ §4.2 color tokens — installed
- ✅ §4.3 typography — installed
- ✅ §4.4 motion — pulse-dot, spinner, reduced-motion
- ✅ §4.5 component library — Button, Pill, Spinner, Toast, Modal, EmptyState, ErrorState, Skeleton
- ✅ §10 chat surface — composer, message list, receipts, surveys, diffs, critic rounds
- ✅ §10.4.1 Survey cards — implemented
- ✅ §10.5 Diff cards — placeholder rendering
- ⚠️ §8.2.1 default flow — partially implemented (chat is the wizard; project shell creation is a no-op for Plan 01)
- ⚠️ Mode toggle — UI placeholder only; tier filtering is Plan 02
- ⚠️ Atelier theme — paper grain background is *not* installed in Plan 01 (added in Plan 03 marketing pages where it earns its keep)
- ⚠️ Branch awareness — none yet (Plan 04)

Placeholder scan: searched for "TBD", "TODO", "implement later", "fill in details" — none found in step content.

Type consistency: `Survey`, `Question`, `QuestionKind`, `Diff`, `CriticRound` defined in `types/chat.ts`. SurveyCard / DiffCard / CriticRoundCard consume those exact types. Receipt's props are independent.

Scope: this plan produces a complete, testable artifact (shell + mock chat). Doesn't depend on any Plan 02+ work. Future plans extend rather than rework.
