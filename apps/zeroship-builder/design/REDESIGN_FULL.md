# zeroship — full redesign (Atelier system, all pages)

Companion to `redesign-preview.html` (home) and `workspace-preview.html`
(workspace shell). This document covers **every page**, the **template
catalog**, and the **3-step creation wizard**.

Companion mockup: `complete-redesign.html` — one scrollable HTML file with
every page below it. Open in a browser to navigate.

---

## Page inventory (18 surfaces)

### Creator surface

```
00  Cover · system specimen (palette, type, motifs)
01  Home · landing for signed-in creators
02  Templates · gallery of starting points
03  Wizard · 3 steps to a new project
04  Workspace · shell with preview + desk-side chat
05  Files · the manuscript view (focused editor)
06  Logs · the ledger (recent events, plain-language)
07  Env · the key cabinet (vars + secrets)
08  Settings · project details + danger zone
09  Account · profile + plan + usage
10  Auth · login & signup
11  Celebration · the "it's live" moment
```

### Admin surface (platform operator)

```
12  [Admin] Library · system overview
13  [Admin] Apps · directory of every app
14  [Admin] App detail · one app's full ledger
15  [Admin] Users · directory of creators
16  [Admin] Revenue · billing & platform fees
17  [Admin] Logs · cross-app system journal
```

---

## Template catalog (twelve starting points)

The wizard's first step. Grouped, not ranked — creators bring different
intents.

### Sharing — make something for friends or a community

| Template | Tagline | What's included |
|---|---|---|
| **Recipe Journal** | _A small space for recipes & photos._ | Posts with photos + reactions + simple sign-in |
| **Photo Album** | _Pictures with captions, like a real one._ | Upload + caption + ordered grid |
| **Reading Room** | _A list shared with a few people._ | Books, status, ratings, shared-with friends |
| **Wedding Page** | _RSVPs, photos, the day's details._ | Hero, RSVP form, photo gallery |

### Collecting — gather information from people

| Template | Tagline | What's included |
|---|---|---|
| **Newsletter Signup** | _An email list with a small archive._ | Email capture + past-issues page |
| **Booking** | _An appointment calendar that doesn't suck._ | Calendar + slot reservation + email confirm |
| **Survey** | _Ask a question, see the answers._ | Form builder + submission view |

### Selling — take money

| Template | Tagline | What's included |
|---|---|---|
| **Tip Jar** | _One-time payments via Stripe._ | Stripe Checkout, custom amount, thank-you page |
| **Subscription** | _Monthly support, with tiers._ | Stripe recurring, gated content for members |
| **Storefront** | _A small shop with cart & checkout._ | Products, cart, Stripe Checkout |

### Showing — present something polished

| Template | Tagline | What's included |
|---|---|---|
| **Portfolio** | _Show your work, beautifully._ | Hero, project grid, about page |
| **Personal Page** | _One page that's just you._ | Hero, links, contact |

### Plus — the always-available exit

```
[ Blank canvas — describe your own → ]
```

---

## The wizard (3 steps)

Linear, progressive, never modal. Each step is a full editorial page so
nothing feels cramped or pop-up-y.

### Step 1 · Pick a starting point

```
        STEP 1 OF 3 · ────  · ────

        Begin a new project.

        Pick a starting point — or start with a blank page.

        [filter chips:  ALL  ·  SHARING  ·  COLLECTING  ·  SELLING
                        SHOWING  ·  INTERNAL  ·  BLANK ]

        ┌─────────────┐  ┌─────────────┐  ┌─────────────┐
        │ № 01        │  │ № 02        │  │ № 03        │
        │ Recipe      │  │ Newsletter  │  │ Tip Jar     │
        │ Journal     │  │ Signup      │  │             │
        │ _A small…_  │  │ _An email…_ │  │ _One-time…_ │
        │ ─────────   │  │ ─────────   │  │ ─────────   │
        │ photos      │  │ archive     │  │ stripe      │
        │ sign-in     │  │ list        │  │ thank-you   │
        │ reactions   │  │ confirm     │  │ custom amt  │
        │ ~ 30s       │  │ ~ 25s       │  │ ~ 1m        │
        │       USE → │  │       USE → │  │       USE → │
        └─────────────┘  └─────────────┘  └─────────────┘
        … nine more …

        Or describe what you want →
```

### Step 2 · Make it yours

When a template was picked, the prompt is pre-filled with the template's
default — phrased as a draft the creator edits.

```
        STEP 1 ✓ · STEP 2 OF 3 · ────

        Recipe Journal · _change_

        Tell me anything different about yours. The agent will read this
        and adjust the design, copy, and behavior.

        ┌──────────────────────────────────────────────────────────────┐
        │ NOTES                                                         │
        │ ─                                                             │
        │ │ A recipe journal for our supper club. We meet monthly.     │
        │ │ Members vote on who hosts next, and the highest-rated       │
        │ │ recipe of the month gets pinned. Make it feel a bit warm    │
        │ │ — like a printed cookbook.                                  │
        │ ▮                                                             │
        └──────────────────────────────────────────────────────────────┘

        Project name             URL preview
        ┌─────────────────────┐  supper-society.zeroship.app
        │ Supper Society      │
        └─────────────────────┘  rename →

        BACK ←                                          NEXT →
```

### Step 3 · Begin

```
        STEP 1 ✓ · STEP 2 ✓ · STEP 3 OF 3

        Ready when you are.

        ┌──────────────────────────────────────────────────────────────┐
        │ JUST BEFORE WE BEGIN                                          │
        │                                                               │
        │  · Recipe Journal, with your supper club specifics            │
        │  · Live at supper-society.zeroship.app                        │
        │  · Free plan — no payment until you choose to add one         │
        │                                                               │
        │  Usually under a minute. You can iterate after — just talk    │
        │  to the agent about anything you want to change.              │
        │                                                               │
        │                                          [  BEGIN →  ]        │
        └──────────────────────────────────────────────────────────────┘
```

---

## Per-page design notes

Each section in `complete-redesign.html` is laid out at viewport height
with a marginalia label `[ № xx · name ]` — like a magazine page-corner.

### 04 Workspace

- Preview is the whole left/main area (1fr)
- Chat is the right "desk" (440px)
- Dev tabs (`files | logs | env | settings`) demoted to a quiet drawer
  at the foot of the preview labelled "_If you're curious:_"
- Status strip in topbar shows concrete progress, not "running tool…"

### 05 Files

The "manuscript view." Single file in focus, file tree as a side
shelf, code shown in JetBrains Mono with the framing chrome staying
in serif. Line numbers in tomato. Margin on the right shows file
metadata (size · path · modified).

### 06 Logs

A ledger. Numbered rows, timestamp in serif-italic, level small-caps.
Plain-language messages. Filter chips at top, calm not alarming.

```
№ 014 · 12:14:02 · INFO    Booted worker, loaded bundle (380ms)
№ 015 · 12:14:08 · INFO    Served / · 14ms · 200
№ 016 · 12:14:11 · WARN    OpenAI returned 429 · retried in 800ms
№ 017 · 12:14:12 · INFO    Served /api/chat · 1.4s · 200
```

### 07 Env

A key cabinet. Each row is one key with name, value (masked for
secrets), and a small "show" / "edit" affordance. Two sections:
**variables** (visible, like CONTROL_URL) and **secrets** (locked,
like OPENAI_API_KEY). A reassuring lede above each: "Variables are
fine to share. Secrets stay private — even from us."

### 08 Settings

Editorial form. Wide left margin with the section name + helper copy,
right side has the inputs. Sections: General · Domain · Plan ·
Danger zone (delete/transfer at the bottom, framed with tomato hairline).

### 09 Account

Big serif name. Email below. Plan badge with usage bar. Billing
preview. Sign-out at the foot.

### 10 Auth

Single column, no card chrome. Editorial heading. Email + password
form, then "or — continue with Google" with hairline divider. Warm
copy ("Welcome back" / "Make something").

### 11 Celebration

The deploy moment as a product event. Ink-black banner, copyable URL
pill, four follow-on actions: open · custom domain · share · price.

### 12 Admin · Library overview

The atelier system continued — but admin is the **library ledger**.
Slightly denser, slightly more tabular. Same paper, same ink, same
tomato — but motifs lean toward a card-catalog/reading-room rather
than a personal notebook.

```
┌── KPIs across the top ────────────────────────────────────────┐
│ APPS         USERS        MRR           PLATFORM FEE TODAY    │
│   1,247        892        $14,302       $2,145                │
└────────────────────────────────────────────────────────────────┘

Recent activity                                    System pulse
─────────────────────                              ──────────────
№ 4081 · supper-society shipped · 2 min            workers   ●
№ 4080 · charlotte signed up · 3 min               control   ●
№ 4079 · marigold-co. earned $42 (Stripe) · 5 min  gateway   ●
…                                                  pg pool   ●
```

### 13 Admin · Apps directory

Numbered table. Columns: №, name, owner, plan, status, last deploy,
actions. Search top-right, filter chips top-left. Click row → 14.

### 14 Admin · App detail

Three-pane: left navigation (overview · deploys · logs · audit · env ·
billing), center detail, right marginalia (IDs, hashes, internal
links). Editorial and forensic.

### 15 Admin · Users directory

Same table format as apps. Columns: №, name/email, joined, plan, apps
count, MRR, actions (impersonate · suspend · view).

### 16 Admin · Revenue

MRR chart up top (simple bar series), per-creator revenue table below,
platform fee accumulation in the right margin.

### 17 Admin · Logs

Cross-app system journal. Same ledger format as creator logs but with
an extra "app" column. Filter by app, level, time, request id.

---

## Design system additions for admin

- **Numbered rows**: each table row has a left-margin `№ 4081` running
  number. Replaces left-padding gutter, gives weight without ornament.
- **Hairline tables**: rules between rows are 1px `--rule`, no zebra
  striping, no heavy headers.
- **Inline status pills**: `live` / `draft` / `suspended` / `errored`,
  small-caps + dot indicator + tomato when relevant.
- **Marginalia for IDs**: technical IDs (UUIDs, hashes) live in the
  right margin, in `JetBrains Mono`, 11px, `--pencil`. Copyable on
  click.
- **Chart aesthetic**: hand-ruled, thin lines, no fills. Like a 19th-
  century ledger. One tomato accent on the active series.

---

## What doesn't change

- The byte fast-path through the runtime (perf foundation)
- The deepagents agent loop (the AI that does the work)
- The control-plane API (apps, deploys, logs, env, sessions)

This is purely the surface. The product underneath is unchanged.

---

## Migration plan (later)

Phase 1: Replace `index.css` with the Atelier tokens + load Fraunces
+ Public Sans. Migrate `Home`, `Login`, `Signup`. (~1 day)

Phase 2: Migrate `ProjectWorkspace` shell + `Chat` + `ToolCall` to
the desk + receipts model. Demote dev tabs to the drawer. (~2 days)

Phase 3: Build the wizard (`/new`) + the templates page (`/templates`).
Wire the template defaults into the agent's first message. (~2 days)

Phase 4: Migrate `Files`, `Logs`, `Env`, `Settings`, `Account` to the
new system. (~2 days)

Phase 5: Build the admin surface (currently lives in `web/dashboard/`)
into a `/admin/*` namespace inside zeroship-builder, gated by a
platform-operator role. (~3 days)

Total: ~10 days for full production-grade rollout.
