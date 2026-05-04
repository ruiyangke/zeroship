# zeroship-builder — full feature inventory

**Status:** working document, pre-spec. Pending discussion before promoted to design.
**Date:** 2026-04-30
**Replaces:** the current `apps/zeroship-builder/` implementation, which is a partial atelier-themed prototype with several non-functional surfaces.

---

## Foundation decisions (locked through brainstorming)

| # | Decision | Choice |
|---|----------|--------|
| 1 | Scope | Full-spec — all surfaces in one comprehensive design, phased implementation |
| 2 | Persona | **C** — one product, two modes: *Ali* (non-technical maker) default, *Sam* (technical) reveal |
| 3 | Brand | **A** — Refined Atelier: keep editorial soul, fix execution. Operational surfaces use neutral sans + normal-case labels. Atelier flourishes (Fraunces, paper grain, italic accents) reserved for marketing + celebration moments. Add green for success — tomato is *only* primary action |
| 4 | Workspace IA | **2** — single shell, swappable canvas. Chat right-rail constant; canvas changes via top-bar pills (preview / files / data / media / logs / env / settings / **plan** / **health**) |
| 5 | Chat surface | **A** — living-document right rail. Linear conversation, resizable, collapsible, multi-modal input, big stop, regenerate, edit-resend, slash, @mentions, ⌘+Enter to send (Enter for newline) |
| 6 | Product shape | **Multi-agent**: Builder + PM + SRE + Reviewer agents share project state. zeroship is not "AI builds your app" — it is "AI builds AND OPERATES your app — a tiny product team in software." |

---

## Multi-agent architecture

zeroship-builder is a fleet of AI agents per project, sharing state through the control plane.

| Agent | Role | When active |
|-------|------|-------------|
| **Builder** | Writes code in response to creator prompts. Loads relevant skills (see BB). | Whenever the creator chats; also when SRE/PM kick off tasks |
| **Critic** | Reviews Builder output across quality dimensions (correctness, security, perf, a11y, UX, code health). Produces structured feedback. Runs the Critic ⇄ Builder revise loop. | Every code-generation turn (paired with Builder) |
| **Reviewer** | CI-style gate before merge: compile / lint / tests / sensitive-file scope / regression risk. Cheap, fast, deterministic-ish. | Triggered by every Builder commit (human-prompted or agent-prompted) |
| **PM** | Maintains backlog, issues, milestones, deployment log. Suggests next steps. Generates digests. | Always running, persistent per project |
| **SRE** | Monitors deployed apps. Detects anomalies. Files bugs. Proposes fixes. | Continuous, polls metrics + uptime |

**Quality boundary:** Critic is the *deep* reviewer that loops with Builder during generation (multiple rounds, aiming for high-quality output); Reviewer is the *fast* gate that runs once before merge (catches obvious unsafe / broken changes). Both are needed; they answer different questions.

**Shared state** (control plane): file tree, deployment history, logs, metrics, conversation history, issues, backlog, milestones, audit log, **quality scorecard history**.

This shape changes the framing of every surface. The chat rail talks to Builder by default but can be addressed to PM (`@pm what's next?`) or SRE (`@sre why is the app slow?`). The notification panel surfaces all agents' work. The audit log shows who-did-what across humans and agents — including which Critic round caught what.

**Decisions resolved (all defaults accepted):**

| # | Decision | Choice |
|---|----------|--------|
| 7 | Mode/tier model | **B** — three tiers: Maker / +Data / +Code with Ali-friendly labels |
| 8 | End-user auth UX | Hybrid: AI writes the code; creator configures providers via UI panel |
| 9 | Templates publishing | V1 curated only; creator-publishing V2+ |
| 10 | Regions | V1 single global; expose multi-region if customers demand |
| 11 | GitHub import | V2+ |
| 12 | 2FA, GDPR export | V1.5 (post-launch, pre-monetization) |
| 13 | Compliance posture in V1 | SOC2 / GDPR copy *on the marketing page*; full SOC2 audit deferred |
| 14 | 15% revenue share disclosure | Stated on pricing page; surfaced at first deploy; reaffirmed at first earning |
| 15 | SRE autonomy default | **Manual** (always ask) for V1; per-project low-risk-auto in V1.5 |
| 16 | PM voice | PM speaks only inside the plan canvas; in chat only when `@pm` mentioned |
| 17 | Skill catalog visibility | Public — surface prominently in marketing as the answer to "what can it build?" |
| 18 | Theme application | Goes through deploy/rollback pipeline; preview-before-apply mandatory |
| 19 | Feature-set dependencies | Auto-install required deps with a heads-up line in the receipt |
| 20 | Agent attribution | Every action labelled (human / Builder / Critic / Reviewer / PM / SRE) in audit log |
| 21 | Domain pack scope | Composition — domain packs pull capability skills together; no standalone domain code |
| 22 | Skill update propagation | Opt-in with in-app prompt ("payments skill updated — migrate?") |
| 23 | Critic loop iterations | Default 3; per-project setting (1–10): fast / balanced / thorough |
| 24 | Public scorecard | Opt-in, full transparency (A–F shown if opted in); public showcase listing requires ≥ B avg |
| 25 | Hard gate overridability | **None** — security / secrets / compile / typecheck / critical CVE are non-negotiable |
| 26 | Scorecard weighting | V1 single global weight; per-project tuning V2 |
| 27 | Critic ⇄ Builder tiebreaker | Critic outranks for *blocking* issues (security, correctness); Builder outranks for *style* (architecture, naming) |

---

## Priority legend

- **[0]** Core build loop — must work or there is no product
- **[1]** Production-grade — required for shipped apps to be usable beyond demo
- **[2]** Trust & growth — marketing, billing, payouts, account
- **[3]** Network / marketplace — discovery, social, public profiles
- **[4]** Collaboration — teams, comments, shared editing
- **[5]** Polish / nice-to-have — dark mode, i18n, voice input

Priority is for *implementation phasing*, not for *spec depth*. Every feature in this list should appear in the eventual design at some level of detail.

---

## A · Pre-auth / public surfaces

1. [2] Marketing landing page (hero, value prop, demo loop, social proof, CTA)
2. [2] Pricing page (plans, fees, examples, FAQ)
3. [2] Public templates gallery (browseable without login)
4. [3] Public showcase / discovery feed
5. [2] Blog / changelog
6. [2] Documentation / guides hub
7. [2] Help / FAQ
8. [2] Legal: Terms, Privacy, Acceptable Use
9. [2] Status page link
10. [2] Contact / support entry
11. [2] About / company

## B · Auth (creator)

12. [0] Sign up — email + password
13. [0] Sign up — Google OAuth
14. [1] Sign up — GitHub OAuth (Sam wants this)
15. [0] Sign in — same providers
16. [0] Email verification
17. [0] Forgot password / reset
18. [1] Magic-link login
19. [1] 2FA (TOTP)
20. [1] Session management (active devices, revoke)
21. [1] Account deletion
22. [1] GDPR data export
23. [2] Connected accounts management

## C · Onboarding / first-run

24. [0] Welcome state for brand-new account (no projects yet)
25. [0] First-prompt magic moment (one prompt → one ship in < 60 s)
26. [1] Optional product tour (skippable)
27. [1] "What are you trying to build?" intent question (steers prompts/templates)
28. [1] First-deploy celebration
29. [1] Empty-state nudges across surfaces
30. [2] Onboarding-to-monetization arc (when to mention payouts)

## D · Project lifecycle

31. [0] Create from prompt
32. [0] Create from template
33. [1] Create by duplicating an existing project
34. [2] Create by importing from GitHub repo
35. [0] Project list / gallery
36. [1] Search projects
37. [1] Sort projects (recent / name / status)
38. [1] Filter projects (live / draft / archived)
39. [1] Pin / favorite projects
40. [2] Tags or folders
41. [1] Archive / unarchive
42. [0] Delete project (with double-confirm)
43. [2] Transfer ownership
44. [0] Rename project
45. [1] Tagline / description (saved!)
46. [1] Project icon / cover image
47. [2] Visibility: private / unlisted / public
48. [2] Public-project README

## E · Workspace shell

49. [0] Top bar: brand, project crumb, mode toggle, URL pill, account
50. [0] Canvas pills (preview / files / data / media / logs / env / settings) — visibility per tier
51. [0] Right-rail chat — resizable + collapsible
52. [0] Mode/tier toggle (Maker / +Data / +Code)
53. [1] Command palette ⌘K — go to project, file, run command, switch canvas, search
54. [1] In-context project switcher
55. [1] Notifications panel (deploys, errors, mentions)
56. [1] Status pulse — live URL + health indicator
57. [1] Help (? button → contextual docs)
58. [1] Keyboard-shortcut overlay
59. [1] Toast / snackbar system
60. [1] Modal / dialog system

## F · Chat surface (right rail)

61. [0] Send text prompt
62. [0] Drag / paste / upload images
63. [1] Drag / upload files (PDFs, JSON, code)
64. [0] Streaming responses with cursor
65. [0] Tool receipts in plain language
66. [1] Receipt expand → input/output JSON
67. [1] Inline diff cards for code changes
68. [1] Code blocks: syntax highlight + copy
69. [0] Big, obvious Stop button while working
70. [0] Regenerate last assistant turn
71. [0] Edit prior user message → resend (truncates after)
72. [1] /slash commands (/deploy, /explain, /test, /undo)
73. [1] @mentions (@server.ts, @env, @route)
74. [0] Conversation persisted per project
75. [1] Conversation history browser (past sessions)
76. [1] Clear conversation (with confirm)
77. [2] Export conversation as markdown
78. [2] Voice input (whisper)
79. [0] Resize chat rail
80. [0] Collapse to strip (36 px)
81. [0] Status indicators: thinking / tool-running / deploying
82. [0] Inline error messages with retry
83. [1] Multi-turn agent loop visibility (steps tree)
84. [2] Cost / token meter (creator-facing)

### F.1 — Agent-generated surveys (clarifying questions)

84.1. [0] `Survey` / `Question` / `QuestionKind` / `Option` / `SurveyResponse` type contract — platform-defined, agent-generated content
84.2. [0] Native `ask_survey` tool exposed to Builder (and reusable by SRE / PM / Critic)
84.3. [0] `<SurveyCard>` renderer — handles all 7 question kinds: single_choice / multi_choice / short_text / long_text / yes_no / scale / image_upload
84.4. [0] Cap: ≤ 3 questions per survey (Builder-prompted, Critic-enforced, renderer-truncated)
84.5. [0] Skip path always available unless every question is `required: true`
84.6. [1] Defaults applied when user skips (Builder must specify)
84.7. [1] Multi_choice with min/max constraints (renderer enforces client-side)
84.8. [1] Single_choice options ≤ 6 enforced; > 6 collapses to `<Select>` dropdown
84.9. [1] Defensive renderer: unknown question kinds fall back to `short_text`; never throws
84.10. [0] Survey + response persisted in `chat_messages.tools_jsonb` (already part of the chat-tool persistence model)
84.11. [0] Submitted survey collapses to a one-line summary in chat ("Answered: just my supper club · cozy/warm")
84.12. [0] User-facing prose summary appended to chat as a normal message (so future turns read naturally)
84.13. [1] Same primitive used for: wizard clarifications · mid-build forks · pre-deploy destructive confirms · SRE post-incident reviews · feature-set configuration
84.14. [2] `survey.shown` / `survey.answered` / `survey.skipped` analytics events (per §28)

## G · Preview canvas

85. [0] Embedded iframe of deployed app
86. [0] Live URL displayed prominently, copy-to-clipboard
87. [0] Reload button
88. [0] Open in new tab
89. [1] Device frame switcher (desktop / tablet / phone)
90. [1] Click-to-edit overlay (click element → "change this" prefilled in chat)
91. [2] Inspect / open built-in devtools (Sam)
92. [0] Auto-reload after deploy
93. [0] Pre-deploy state ("Nothing's been built yet")
94. [0] Failed-deploy state with link to logs
95. [0] Build / deploy progress indicator
96. [1] Auth-as-test-user (preview as logged-in end user)
97. [2] Performance overlay (LCP, CLS)

## H · Files / editor canvas

98. [1] File tree with folders, expand/collapse
99. [1] Tree search
100. [1] Open file in editor
101. [1] Tab strip for open files
102. [1] Split editor (side-by-side)
103. [1] Monaco editor — TS LSP, IntelliSense, autocomplete
104. [1] Save (Cmd+S, autosave option)
105. [1] Diff view vs last deploy
106. [1] Undo / redo
107. [1] Find / replace in file
108. [1] Find in project
109. [1] Create file / folder
110. [1] Rename
111. [1] Delete (confirm)
112. [1] Move (drag-and-drop)
113. [1] Upload binary asset
114. [1] Download file
115. [2] Download project as `.zship` archive
116. [1] Right-click context menu
117. [2] Read-only mode (for shared / preview)

## I · Logs canvas

118. [1] Real-time log stream (SSE / WS)
119. [1] Filter by level (info / warn / error)
120. [1] Filter by source (request / function / build)
121. [1] Search within logs
122. [1] Time-range picker (last hour / today / custom)
123. [1] Pause / resume tail
124. [1] Auto-scroll to latest
125. [1] Expand entry → full payload
126. [1] Copy log line
127. [2] Export logs (CSV / JSON)
128. [1] Real timestamps with timezone
129. [1] Color-coded levels

## J · Data canvas (DB)

130. [1] Browse tables / collections
131. [1] Schema viewer (Sam tier only)
132. [1] Notion-style row list (Ali tier)
133. [1] Add row inline
134. [1] Edit row inline
135. [1] Delete row (confirm)
136. [1] Search / filter rows
137. [1] Sort by column
138. [1] Pagination / virtualization
139. [2] Foreign-key navigation
140. [2] SQL editor (Sam tier)
141. [2] Saved queries
142. [2] Export table (CSV / JSON)
143. [2] Import data (CSV)
144. [1] Create table (via prompt or UI)
145. [2] Drop table (Sam, confirm)

> **Extended in section HH (Data management) and II (Branching).**

## K · Media canvas (storage)

146. [1] Browse buckets / paths
147. [1] List uploaded files
148. [1] Upload (drag / click)
149. [1] Preview image / video / audio
150. [1] Download
151. [1] Delete (confirm)
152. [1] Copy public / signed URL
153. [1] Storage usage display

## L · Env canvas

154. [1] List variables (key / value / last-updated)
155. [1] Add / edit / delete variable
156. [1] List secrets (key only, value masked)
157. [1] Add secret
158. [1] Rotate secret
159. [1] Delete secret
160. [2] Per-environment vars (dev / preview / prod)
161. [2] Audit log of env changes

## M · Settings canvas

162. [0] Project name (with save)
163. [1] Tagline / description (with save)
164. [1] Project icon
165. [1] Visibility toggle
166. [0] Default URL (subdomain)
167. [1] Custom domain — add, DNS verification, TLS auto-provision
168. [2] Multiple custom domains
169. [1] Plan for this project (free / pro / enterprise)
170. [1] Spending limit + alerts
171. [1] Resource overview (CPU, mem, requests, storage)
172. [2] Region / deploy target
173. [2] Build settings override
174. [2] Webhooks / integrations
175. [2] Transfer ownership
176. [1] Archive / unarchive
177. [2] Export project bundle
178. [1] Delete project (danger zone, double-confirm)

## N · Deploy / versions

179. [0] Auto-deploy on AI-completion
180. [1] Manual deploy button
181. [1] Deploy history (timestamps, summary, hash)
182. [1] Rollback to previous deploy
183. [2] Preview branches (deploy alt versions to subdomains)
184. [1] Build / deploy logs
185. [1] Deploy status flow: queued → building → deploying → live / failed
186. [0] Live banner / celebration after first ship
187. [2] Compare two deploys (diff)

## O · End-user auth (for the BUILT app)

188. [1] Configure providers for built app (email, OAuth)
189. [1] End-user list / management
190. [1] Disable / ban end users
191. [2] End-user data export
192. [1] Custom branding on built-app login pages

## P · Templates

193. [2] Public template gallery
194. [2] Categories / tags
195. [2] Search templates
196. [2] Template detail page (description, screenshots, demo URL)
197. [2] "Use template" → create project
198. [3] Submit your project as a template
199. [2] Featured templates curation

## Q · Account / profile

200. [0] Display name + avatar
201. [0] Email (verified)
202. [1] Bio
203. [2] Public creator portfolio page
204. [2] Social links
205. [1] Change password
206. [1] Delete account
207. [1] Connected accounts (Google, GitHub)

## R · Billing — creator (paying zeroship)

208. [2] Current plan + usage
209. [2] Plan comparison + upgrade / downgrade
210. [2] Add payment method
211. [2] Default card
212. [2] Invoices / receipts list
213. [2] Download invoice (PDF)
214. [2] Usage breakdown by project
215. [2] Spending limits + alerts
216. [2] Cancel subscription

## S · Payouts — creator (earning from end users)

217. [2] Stripe Connect onboarding
218. [2] Bank account / payout details
219. [2] Payout schedule
220. [2] Earnings dashboard (current + historical)
221. [2] Per-project earnings
222. [2] Platform fee (15%) breakdown
223. [2] Earnings export
224. [2] Tax forms (1099 / W-9)

## T · Built-app monetization tooling

225. [2] Stripe Checkout integration (subs + one-time)
226. [2] Pricing page templates for built apps
227. [2] Customer portal for built apps
228. [3] Coupons / discounts
229. [3] Free trial config

## U · Sharing / social

230. [2] Share project URL
231. [2] Auto-generated screenshot / OG card for built app
232. [2] Social meta tags
233. [3] Public showcase opt-in
234. [3] Star a public project
235. [3] Follow a creator
236. [3] Activity feed

## V · Collaboration (later)

237. [4] Invite collaborators by email
238. [4] Roles (owner / editor / viewer)
239. [4] Comments on chat msgs / files
240. [4] Live cursors
241. [4] Shared chat history with attribution
242. [4] Activity log per project

## W · Notifications

243. [1] In-app notification center
244. [1] Email notifications
245. [1] Notification preferences
246. [1] Mark all read

## X · Search

247. [1] Global search (projects, files, chats)
248. [2] Recent searches

## Y · Admin (real, role-gated)

249. [1] Role-based access control (admin role)
250. [2] App list with details
251. [2] User list with details
252. [2] Suspend / unsuspend user or app
253. [2] Refund / credit
254. [2] Revenue dashboard, MRR, ARR, churn
255. [2] System health (workers, control, gateway, DB pool)
256. [2] Live metrics: req/s, p50/p99, error rate
257. [2] Audit log
258. [2] Impersonate / view-as-user
259. [3] Moderation queue (reported public projects)
260. [3] Feature-flag management

## Z · Help & support

261. [2] In-app docs hub
262. [2] Searchable help center
263. [2] Contextual help triggers
264. [3] Live chat support
265. [2] Submit bug / feedback
266. [2] Status banner in-app when degraded

## BB · Builder skill catalog (what the AI knows how to build)

The Builder agent loads relevant skills based on the prompt. Skills are organized as **domain packs** (vertical, e.g. CRM) and **capability skills** (horizontal, composable, e.g. payments). Surfaced to creators as part of marketing copy and onboarding ("what can zeroship build?").

### Domain packs (vertical)

276. [0] Todo / list / personal productivity
277. [0] Notes / wiki / knowledge base
278. [1] Blog / content publishing
279. [1] Newsletter / email signup
280. [1] Landing page / waitlist
281. [1] E-commerce (products, cart, checkout)
282. [1] CRM (contacts, deals, pipelines)
283. [2] Marketplace (listings, transactions, reviews)
284. [2] Social (posts, follows, feed)
285. [2] Forum / community
286. [1] Booking / scheduling
287. [1] Survey / form
288. [1] Analytics dashboard
289. [2] Internal tool / admin panel
290. [2] Real-time chat / messaging
291. [2] File sharing
292. [1] Subscription SaaS
293. [1] Gallery / portfolio
294. [1] Event / RSVP
295. [2] Quiz / test
296. [0] Calculator / utility / generator

### Capability skills (horizontal, composable)

297. [0] Auth (email / OAuth / SSO)
298. [1] Payments (Stripe subs + one-time)
299. [1] Email (send transactional)
300. [0] File upload / storage
301. [1] Search (full-text + semantic)
302. [1] AI-in-app (LLM calls from built apps)
303. [2] Real-time (WebSockets, presence, live updates)
304. [2] Scheduling (cron jobs)
305. [1] Notifications (push / in-app / email)
306. [2] Maps / location
307. [2] Charts / visualization
308. [1] Rich text editor
309. [0] Forms / validation
310. [3] Multi-language i18n
311. [2] Image / video processing
312. [2] PDF generation
313. [1] CSV import / export
314. [2] External API integration patterns

### Skill catalog UI

315. [1] "What can zeroship build?" public page — landing page section + standalone page, backed by the catalog
316. [1] Skill picker in onboarding ("what kind of app?")
317. [1] Skills auto-loaded based on prompt content (no UI needed for activation)
318. [1] Per-project view: which skills are *active* in this app
319. [2] Skill packs as marketplace items (community contributions, V2+)
320. [2] Per-org private skill packs (enterprise)

---

## CC · AI PM agent — issues, features, milestones, deployments

The PM agent maintains the SDLC for each project. Creator-facing canvas: **plan** pill (visible from Maker tier — Ali wants to see what's planned, not just what shipped).

### Issues

321. [0] Issue object: title, description, status (open / in-progress / closed), severity (low/medium/high/critical), source (user / SRE / PM / chat), created/updated, assignee (agent or human)
322. [0] Issues list view (filterable: open / closed / mine / mentioned)
323. [0] Issue detail view (description, comments, linked deploys/PRs, status timeline)
324. [0] Create issue manually
325. [1] AI auto-creates from chat ("you said the form was broken — I'll log that") with confirm-or-edit
326. [0] AI SRE auto-creates from detected bugs (see EE)
327. [1] Comments / discussion thread (humans + agents)
328. [1] Link issues to deployments
329. [2] Tag / label issues
330. [2] Search across issues
331. [2] Issue templates (bug / feature request / question)

### Features (backlog)

332. [1] Proposed feature: title, description, status (proposed / planned / building / shipped / dismissed)
333. [1] PM agent proactively suggests features based on usage patterns and gaps in common functionality for the app's domain
334. [1] Creator can promote / dismiss / edit suggestions
335. [1] Approving a feature kicks off a Builder agent task automatically
336. [2] Feature voting (when collaboration ships — V4)

### Milestones

337. [1] Group features and issues into milestones (e.g. "v1.0 launch", "Public beta")
338. [1] Optional target date
339. [1] Auto-computed progress %
340. [1] PM-suggested milestones based on project trajectory
341. [2] Milestone retrospective — auto-generated summary on milestone completion

### Deployments log

342. [0] Deployment record: timestamp, build hash, summary, linked issues/features, status, duration, who triggered
343. [1] Click a deploy → see what changed (diff vs prior)
344. [1] Rollback from deploy detail
345. [1] PM-generated changelog per deploy ("v0.4 — added voting, fixed login bug")
346. [2] Compare two deployments side-by-side
347. [2] Public changelog page for the built app (auto-generated)

### PM agent voice

348. [1] Daily / weekly digest in the plan canvas: "Here's what's planned, in flight, stuck"
349. [1] Replies in chat when @mentioned: "@pm what's next?", "@pm when did login ship?"
350. [2] Email digest (configurable cadence)
351. [2] Status reports for stakeholders (shareable URL of plan canvas in read-only)

### Plan canvas UI

352. [1] Three sub-tabs in plan canvas: **Issues** · **Roadmap** · **Deployments**
353. [1] At-a-glance stats: open issues, in-flight features, last deploy
354. [1] Recent activity timeline (issues filed, features shipped, deploys)
355. [2] Kanban view for features (proposed → planned → building → shipped)
356. [2] List view as alternative

---

## DD · Themes & feature sets (extends Templates)

Three composable building blocks: **Templates** (full apps), **Themes** (visual presets), **Feature Sets** (modular feature packs). Creators mix-and-match.

### Themes — visual presets, applyable to any app

357. [1] Theme catalog (browse, preview)
358. [1] Apply theme → Builder agent rewrites styling layer (CSS, design tokens, type, motion); auto-deploys
359. [1] Preview theme on current app before applying (sandbox preview)
360. [1] Built-in V1 theme set:
   - Refined Atelier (the platform's own brand, available to apps)
   - Minimal mono
   - Playful pastel
   - Dark-tech
   - Editorial newspaper
   - Brutalist
   - Glass / blur
   - Retro 80s
361. [1] Theme = JSON (tokens) + Tailwind config + small CSS overrides — small enough to swap fast
362. [2] Custom theme: creator describes a vibe ("Wes Anderson colors, serif type, lots of whitespace") → AI generates theme JSON
363. [3] Community-published themes (V2+)

### Feature sets — modular feature packs

364. [1] Feature-set catalog (browse, with what-it-adds previews)
365. [1] "Add this feature" → Builder agent integrates it (writes code, creates DB schema, runs migrations); auto-deploys
366. [1] Built-in V1 feature sets:
   - Login + accounts (auth provider config + UI)
   - Stripe payments + subscriptions
   - Comments / reactions
   - Search bar with results
   - Email signup / newsletter
   - File upload + media gallery
   - Social share / OG cards
   - Real-time updates (presence, live counters)
   - Notifications (in-app + email)
   - Multi-language i18n
   - Admin panel for the built app
   - Analytics + visitor tracking
   - SEO / sitemap / structured data
   - Calendar / scheduling
   - Forms / surveys
367. [1] Each feature-set documents what it adds, files touched, dependencies on other sets
368. [1] Dependency resolution ("Comments needs Auth — install both?")
369. [1] Conflict detection ("This payments set conflicts with your existing Lemon Squeezy integration — pick one")
370. [2] Per-project view: which feature-sets are active
371. [2] Remove a feature set — Builder cleans up the integration
372. [3] Community-published feature sets (V2+)

### Templates (extension of P)

373. [2] Templates declare which themes + feature sets they include
374. [2] Templates can be customized at creation time (pick alt theme, toggle optional feature sets)
375. [2] Template authors can mark sections "AI-extensible" with hints

---

## EE · AI SRE agent — health, bugs, auto-resolution

The SRE agent continuously monitors deployed apps and surfaces issues. Creator-facing canvas: **health** pill (always visible — even Ali tier — because "is my app working?" is a universal question).

### Monitoring

376. [1] Uptime probe (every minute via gateway)
377. [1] Latency tracking (p50, p95, p99 per route)
378. [1] Error rate (5xx and 4xx)
379. [1] Resource usage (CPU, memory, DB queries/sec, V8 isolate restarts)
380. [1] Request volume / traffic pattern
381. [2] Custom health endpoints (creator can define `/health` returning structured info)
382. [2] User-defined alerts ("notify me if error rate > 5%")

### Detection

383. [1] Anomaly detection: error rate spike, latency regression, traffic crash
384. [1] Pattern recognition: same error fingerprint N times in M minutes → file a bug
385. [1] Build failure → file a bug with logs attached
386. [1] Slow query / hot endpoint → file a performance issue
387. [2] Security signals (unusual auth patterns, brute-force attempts)
388. [2] Cost anomalies (sudden spend spike — alerts creator)

### Reporting

389. [1] In-app notification when SRE finds something significant
390. [1] Auto-create issue in plan canvas (PM picks it up — see CC)
391. [1] Incident timeline view (start → diagnosis → mitigation → resolution)
392. [2] Email digest of weekly health
393. [2] Public status page generation (per-project, opt-in, for the built app's users)

### Auto-resolution

394. [1] SRE proposes a fix as a draft change (writes the patch in a new branch)
395. [1] Patch goes to **Reviewer agent** for sanity check (does it compile, does it touch unrelated code, does it look risky)
396. [1] Creator gets notification: "SRE found a fix for [bug X]. Review the diff?"
397. [1] Creator approves → auto-merge & deploy via the normal pipeline
398. [1] Auto-rollback if SRE detects fix made things worse (error rate higher post-deploy)
399. [2] **Autonomy levels** per project: *manual* (always ask) / *low-risk auto* (typos, null-checks, log-only) / *full auto* (deploy without asking, notify after)
400. [2] Auto-fix history: list of fixes attempted, what worked, what got rolled back

### Health canvas UI

401. [1] Health pill with at-a-glance status dot (green / amber / red)
402. [1] Status summary: "All good · last incident 4 days ago"
403. [1] Recent incidents timeline
404. [1] Active alerts list (with acknowledge / mute)
405. [1] Performance chart (last 24 h / 7 d / 30 d)
406. [1] "What SRE is watching" — list of monitored signals
407. [2] Cost-to-date and projected monthly spend

---

## FF · Reviewer agent (CI-style gate, fast)

408. [1] Reviewer runs on every Builder commit (human-prompted *or* agent-prompted)
409. [1] Checks: compiles? typechecks? lints? tests pass? touches sensitive files (auth, env, secrets)? scope larger than expected?
410. [1] Generates a one-screen review summary (pass / fail per check) shown alongside the diff
411. [2] Configurable strictness per project (V1 = sensible defaults, V2 = tunable)
412. [2] Reviewer can request human-in-the-loop ("this change rewrites 40% of auth.ts — please confirm before I deploy")

---

## GG · Quality control (Critic agent + scorecard + gates)

The Critic agent and a quality scorecard are the platform's primary defense against AI slop. Critic runs *during* generation, scorecard tracks *over time*, gates block *before* deploy. Together they keep shipped apps from being broken on arrival or rotten by version 30.

### GG.1 — Quality dimensions tracked per project

413. [0] **Correctness** — does it compile, typecheck, pass smoke tests, and behave per the brief
414. [0] **Security** — secret scanning, dep audit, auth enforcement on protected routes, input validation, common vuln patterns (XSS / SQLi / CSRF / SSRF), HTTPS-only, secure cookies
415. [1] **Performance** — first-paint budget, bundle size budget, query patterns (N+1 detection, missing indices), Lighthouse perf score on three viewport sizes
416. [1] **Accessibility** — axe-core ruleset, contrast, keyboard nav, focus visible, screen-reader labels, semantic HTML, WCAG AA target
417. [1] **UX completeness** — every fetch has loading + error + empty states; every form has validation + submit feedback; destructive actions confirmed
418. [1] **Responsive design** — every page renders without horizontal scroll at 375 / 768 / 1280 px viewports; no fixed widths > 100 vw; touch targets ≥ 44 × 44 px; mobile-first Tailwind patterns; forms use correct mobile keyboard input types (email / tel / number / date)
419. [1] **Code health** — type coverage, lint clean, no dead code, naming consistency, file-size limits, function-size limits, no TODO/FIXME left behind
420. [1] **Reliability** (operational, fed by SRE) — uptime, error rate, recent incidents
421. [2] **SEO / discoverability** — title, description, OG card, sitemap, structured data (for public apps)
422. [2] **Content cleanliness** — no lorem ipsum, no placeholder copy, no hardcoded test emails
423. [2] **Compliance hooks** — privacy policy linked, ToS linked, cookie consent if needed, GDPR-relevant flags

### GG.2 — Critic ⇄ Builder revise loop (during generation)

423. [0] After Builder produces a code change, Critic reviews against the dimensions above
424. [0] Critic returns structured feedback: list of issues by dimension, severity, and suggested fix
425. [0] Builder revises based on feedback; Critic re-reviews
426. [0] Loop until Critic approves *or* max iterations reached
427. [1] Default max iterations: **3** (V1). Configurable per project (1–10).
428. [1] Per-project quality preference: **fast** (1–2 iterations, ship sooner) | **balanced** (3) | **thorough** (5+, slower)
429. [1] If max iterations reached without approval, surface to creator: "I'm at iteration limit — here are remaining concerns" + change still ships unless creator blocks
430. [1] Critic feedback is logged and viewable per change ("show what Critic flagged")
431. [2] Token / time budget per loop visible to creator (cost transparency)
432. [3] Critic can request human-in-the-loop for irreducible disagreements ("Builder and I disagree on auth approach — which is right?")

### GG.3 — Pre-deploy gates

Each gate is **hard** (deploy blocked) or **soft** (warning, deploy proceeds with creator confirm).

433. [0] **Hard gate**: build succeeds (no compile errors)
434. [0] **Hard gate**: typecheck passes (no TS errors above threshold)
435. [0] **Hard gate**: no secrets in client bundle (regex + entropy detection)
436. [0] **Hard gate**: dep audit — no critical CVEs in shipped dependencies
437. [1] **Hard gate**: tests pass (unit + smoke)
438. [1] **Soft gate**: Critic score ≥ project minimum (default: 70)
439. [1] **Soft gate**: a11y baseline (no critical axe-core violations)
440. [1] **Soft gate**: performance budget (bundle ≤ 500 kB, first paint ≤ 2.5 s)
441. [2] **Soft gate**: visual regression (screenshot diff vs prior deploy — flag major changes)
442. [2] **Hard gate** for monetization: security score ≥ 80 (must apply before payment-handling code can deploy)

### GG.4 — Post-deploy verification

443. [1] SRE runs smoke tests against the live URL within 60 s of deploy
444. [1] Lighthouse runs against live URL, scorecard updated
445. [1] Real-user-monitoring (first hour error rate vs baseline)
446. [1] **Auto-rollback** if post-deploy error rate > 2× pre-deploy baseline within first 5 minutes
447. [2] Canary / traffic-shadowing for projects on paid plans (10% of traffic to new deploy first)
448. [3] User-feedback signal aggregation (built-app users can report issues)

### GG.5 — Quality scorecard

449. [1] Per-project scorecard surfaced in the **health** canvas (sub-tab: *quality*)
450. [1] Per-dimension score (0–100) → letter grade (A+ through F)
451. [1] Overall score = weighted average; weights configurable per project
452. [1] History view: scorecard over time, deployment-by-deployment
453. [1] Score deltas highlighted ("this deploy dropped a11y from 92 → 78 — see why")
454. [2] **Public scorecard badge** (opt-in) on the built app and on creator's portfolio — like a Yelp rating, but real
455. [2] Scorecard required minimum to publish to public showcase (≥ B average)
456. [3] Scorecard ranking in template marketplace (V2+)

### GG.6 — Skill-level quality budgets

Each skill (per BB) declares its own quality requirements:

457. [1] Auth skill must include: rate limiting on login, CSRF protection on state-changing routes, secure cookie defaults, password hashing (argon2 / bcrypt)
458. [1] Payments skill must include: webhook signature verification, idempotency on charge endpoints, no card data ever stored locally
459. [1] Database skill must include: parameterized queries (no string concat), input validation, query timeout
460. [1] File-upload skill must include: MIME type validation, size limit, virus-scan hook, no executable file types
461. [2] Critic enforces these budgets per skill — Builder cannot ship a code path that uses skill X but skips skill X's quality requirements

### GG.7 — Quality coach (proactive)

462. [2] Critic surfaces *unsolicited* improvement suggestions when not in active code-gen
463. [2] Daily digest: "Your forms could use better error messages; want me to improve them across the app?"
464. [2] Suggestions go through PM agent → become features in the backlog (not deployed without creator approval)
465. [3] Quality nudges in onboarding ("Most successful apps add OG images — want me to generate one?")

### GG.8 — Continuous evaluation of zeroship itself

466. [2] Internal benchmark suite: a fixed set of "build me X" prompts run nightly against Builder
467. [2] Quality scores tracked over time → catch regressions in the *AI itself* (not just shipped apps)
468. [2] Compared against a held-out human-built reference set ("would a competent dev have built it this way?")
469. [3] Public quality leaderboard (per skill / domain) compared to other AI builders

### GG.9 — Quality canvas UI

470. [1] Quality canvas (or sub-tab inside health): scorecard at top, dimension breakdown, recent deploys with score deltas
471. [1] Per-dimension drill-down: list of currently flagged issues, severity, fix suggestion
472. [1] "Run quality check now" button (re-runs Critic against current state)
473. [2] Scorecard widget on home / project gallery cards (small badge per project)
474. [2] Scorecard share URL for showcasing

---

## HH · Data management (extends J)

Deeper data operations that the Notion-row + SQL view don't cover.

### HH.1 — Schema inspection (deep)

475. [1] Schema diagram (visual ER) — tables, columns, foreign keys, click-through navigation
476. [1] Index inspector — name, type (btree / gin / unique), columns, size, last-used timestamp
477. [1] Relations panel — every FK touching the selected table, both directions
478. [1] Constraints panel — unique, NOT NULL, defaults, CHECK constraints, composite keys
479. [1] Column statistics — distinct count, null %, min / max / avg (numeric), top-N values (categorical)
480. [1] Row count + on-disk size per table; index size; total project DB size
481. [1] Per-table migration history (timeline with author, branch, applied_at, brief)
482. [2] Query planner — `EXPLAIN ANALYZE` rendered as a tree inside the SQL editor
483. [2] Triggers / hooks list (zeroship lifecycle hooks: before_insert, after_update, etc.)
484. [3] Data lineage — for a given column, which deploys / migrations touched it
485. [3] Data quality rules — Critic-checked invariants on production data ("no NULL email on contacts", "stage_id must reference a row")

### HH.2 — Bulk + advanced operations

486. [1] Bulk select rows (checkbox column)
487. [1] Bulk edit (set column value across selected rows)
488. [1] Bulk delete (with confirm + undo window)
489. [1] Saved views (named filter + sort + column order, scoped to project)
490. [2] Shared saved views (for collaborators, V4)
491. [2] Computed / formula columns (e.g. `revenue = qty * price`)
492. [2] Custom column types — currency, percent, date+tz, duration, JSON, enum, image, file
493. [2] Conditional formatting rules (cell color / bold by value)
494. [2] Charts on top of data — bar / line / pie generated from a saved view
495. [2] CSV / JSON export filtered or selected (extends 142)
496. [2] CSV import with column-mapping UI (extends 143)
497. [2] Test data generation: "seed me 100 fake contacts" → Builder runs the test-data skill, results land in current branch
498. [2] Soft-delete / undo for last N row deletes (per project, configurable retention)
499. [3] Data anonymization / masking when copying prod → dev (Builder skill)

### HH.3 — Backups + restore

500. [2] Auto backup snapshots — daily on free, hourly on Pro, every 15 min on Enterprise
501. [2] Manual snapshot ("snapshot before I run this big migration") with optional name
502. [2] Point-in-time restore — last 24 h free, 7 d Pro, 30 d Enterprise
503. [2] Snapshot list with size, age, source branch
504. [2] **Restore safety**: a restore *never* blasts the live branch — applies to a new branch first, creator promotes after verification
505. [3] Cross-region snapshot replication

### HH.4 — Migrations as first-class objects

506. [1] Migration object schema: `id, timestamp, author (human / Builder), sql_up, sql_down, status (pending / applied / failed / rolled-back), branch_id, deploy_id`
507. [1] Migrations list per branch (status, author, applied_at, brief)
508. [1] Apply migration — runs on target branch with pre/post safety checks
509. [1] Rollback migration — where reversible (down SQL exists and matches up)
510. [1] Auto-generated migration name + description (Builder writes this)
511. [2] Migration linter — flags breaking changes before apply (drop column, NOT NULL without default, type narrowing, FK without index)
512. [2] Migration preview — runs against a shadow branch, reports impact (rows affected, locks held, est. duration)
513. [2] Multi-step migration recipe — split a breaking change across multiple deploys (e.g. add new column → backfill → flip code → drop old column)
514. [2] Migration *requires* matching code change in same deploy (Reviewer enforces — no schema-only deploys that the code can't yet read)

### HH.5 — Data management UI placement

515. [1] Schema canvas — sub-tab inside Data canvas (`tables · schema · indexes · migrations`)
516. [1] Migrations sub-tab in Data canvas
517. [1] Backup sub-tab inside Settings → Data (or moved into Data canvas)
518. [1] "Run quality check on data" button → triggers Critic data-quality dimension

---

## II · Branching (dev / prod / preview)

A first-class branching primitive across schema, data, env, and deploy state. Inspired by Neon / Supabase branches but tightly integrated with the deploy pipeline and the multi-agent fleet.

### II.1 — Branches as objects

519. [1] Every project has at least two branches: **prod** (default deploy target) and **dev** (default working copy)
520. [1] Branches are named, listable, linkable. Reserved names: `prod`. Auto-generated for previews: `preview-{deploy-hash[:6]}`.
521. [1] Branch object schema: `id, project_id, name, parent_branch_id, created_at, created_by, last_activity_at, ttl_at?, kind (long_lived | preview | snapshot_restore)`
522. [1] Each branch carries: schema, data, env-var overlay, recent-deploy pointer
523. [1] Branch list view — in Data canvas top bar AND in project Settings → Branches
524. [1] Branch metadata: parent, age, size on disk, current row count, last activity, deploy attached
525. [1] Branch switcher widget — switches *which branch the canvas operates on*; Data + Env + Logs canvases follow the active branch

### II.2 — Branch lifecycle

526. [1] Create branch from another branch — instant logical fork (copy-on-write where the platform supports it)
527. [1] Default fork direction: dev forks from prod ("snapshot of prod for safe testing")
528. [1] Rename branch (except `prod`)
529. [1] Delete branch (except `prod`); confirm with row-count + size; preview branches auto-delete after TTL
530. [1] Branch TTL — preview branches default 7 days; long-lived branches no TTL
531. [2] Branch from a specific deploy hash (point-in-time fork)
532. [3] Branch protection — `prod` requires Reviewer approval to receive any merge or destructive migration

### II.3 — Schema and data diff / merge

533. [2] Schema diff between branches (table, column, index, constraint level)
534. [2] Data diff (sampled — too expensive to do full for large tables; row-level diff for tables under N rows)
535. [2] Merge branch — apply schema changes from source to target with safety checks
536. [2] Merge conflicts — schema conflicts shown with resolution UI (rename / drop / keep both)
537. [2] Merge dry-run — Builder simulates the merge against a shadow branch, reports impact

### II.4 — Env vars per branch

538. [1] Per-branch env-var overlay (extends section L env vars) — variables/secrets can be project-level or branch-specific
539. [1] Branch-level secrets shown alongside project-level in Env canvas, with origin marker
540. [2] Convenience: "copy env from prod to dev" with masking option for secrets

### II.5 — Deploy targeting

541. [1] Each project has a *production* branch (default = `prod`); the live URL points there
542. [1] Each branch can have its own deploy URL: `{branch}--{project}.zeroship.app` (e.g. `dev--supper-club.zeroship.app`)
543. [1] Production deploy promotes a branch's recent build to `prod` (atomic gateway flip)
544. [2] **Preview deploys** — every PR-style change kicks off a preview branch + preview URL automatically (extends original feature 183)
545. [2] Promote a preview branch to prod ("ship preview-a1b2c3 to production") — runs full safety pipeline first
546. [3] Multi-region deploy targeting per branch (V2+)

### II.6 — Data branching mechanics (platform-level dependency)

547. [1] Postgres branching support added to `compio-postgres` (logical clones via separate schemas in V1; full Neon-style at-rest CoW in V2)
548. [1] V1 strategy: each branch = separate Postgres schema (`project_<id>__<branch>`) within the per-app database
549. [1] V1 migration application: applies to current schema only; data isolation guaranteed
550. [2] V2 strategy: integration with a CoW Postgres backend (Neon / equivalent) for cheap full-data branching
551. [2] Storage / blob branching — V2; V1 ships shared blob storage across branches (acceptable since blobs are usually larger and rarely diverge)

### II.7 — Branch-aware Builder / Critic / SRE

552. [1] Builder defaults to operating on the active branch; can be addressed `@builder on prod` to target another
553. [1] Critic checks migrations for breaking changes — flags hard if target is `prod`
554. [1] Critic enforces: destructive migrations on `prod` (DROP COLUMN/TABLE) require explicit creator opt-in (cannot be auto-approved)
555. [1] SRE monitors prod by default; can be configured to monitor a non-prod branch (V2)
556. [2] PM tracks deploys per branch; suggested features default to dev branch first

---

## JJ · Cross-platform (web · tablet · mobile)

zeroship-builder runs on three form factors. Apps it builds also target three form factors. Two halves to this section.

### JJ.1 — Builder runs responsively on web / tablet / mobile

557. [0] Responsive web (the builder itself) — works across desktop, tablet-landscape, tablet-portrait, phone
558. [0] Distinct layouts per form factor (not shrunken desktop):
   - Desktop ≥ 1024 px: two-column shell (canvas + chat rail)
   - Tablet landscape 768–1023 px: two-column with narrower chat rail (280 px)
   - Tablet portrait 600–767 px: vertical split (canvas top / chat bottom, drag-resize)
   - Phone < 600 px: chat is home, canvas opens in full-screen overlay via pill strip
559. [1] Standard touch interactions:
   - Long-press = context menu
   - Pinch-zoom in preview canvas
   - Pull-to-refresh on logs canvas
   - Tap-and-hold-to-drag the canvas/chat divider (tablet portrait)
560. [1] Mobile keyboard accessory bar (iOS / Android) with `⌘+⏎`, `📎`, `🖼` shortcuts
561. [1] Bundle budget: initial ≤ 200 KB gzipped (auth + workspace shell)
562. [1] Lazy-load Monaco editor (1.2 MB) — only loads in +Code tier
563. [1] Lazy-load chart libraries (used in plan / health canvases)

**Out of scope (V1 + V2):** PWA installation, service workers, push notifications, offline mode, native mobile apps. Cross-platform here means *responsive web only*. Revisit installable / native if real demand surfaces post-launch.

### JJ.2 — Apps built on zeroship are responsive by default

570. [0] Builder default: mobile-first Tailwind classes in all generated layouts
571. [0] Default project template includes proper `<meta viewport>`, touch icons, picture-element pattern
572. [1] Skills declare `form_factors: [mobile, tablet, desktop]` they support
573. [1] Critic responsive-design dimension (extends GG.1 dim 418): tested at 375 / 768 / 1280 px
574. [1] Hard deploy gate: no horizontal scroll at 375 px viewport
575. [1] Tables auto-generate mobile-card variants
576. [1] Forms auto-generate mobile keyboard input types (`type="email"`, `inputmode="numeric"`)
577. [1] Touch targets ≥ 44 × 44 px enforced by Critic
578. [1] Preview canvas device-frame switcher (desktop / tablet / phone) — already in feature 89
579. [1] Critic post-deploy verification: screenshot per device frame, visual regression diff across all three
580. [2] Templates declare `tested_on: [desktop, tablet, mobile]` in metadata; V1 templates ship with all three tested

**Out of scope (V1 + V2):** native-app generation, PWA wrapping, capacitor wrappers for built apps. Built apps are responsive web. If a creator wants their app installable, they can ship a PWA themselves (the platform doesn't generate one).

---

## AA · Accessibility / responsiveness / polish

267. [1] Mobile responsive (tablet + phone)
268. [1] Keyboard navigation everywhere
269. [1] WCAG AA contrast + focus visible
270. [1] Screen-reader labels
271. [2] Optimistic updates
272. [2] Background sync / retry
273. [5] Dark mode (atelier-night)
274. [5] PWA / offline
275. [5] i18n / locales

---

## Out of scope (intentional — challenge if you disagree)

- **Git integration inside zeroship** (commit / branch UI) — too IDE-heavy; GitHub *import* covers the on-ramp use case (V2)
- **Plugin / extension system** — premature in V1, deferred to community-skill marketplace V2+
- **Real-time multiplayer editing** in V1 — collaboration deferred to V4
- **Third-party integrations marketplace** (Zapier-style)
- **Self-hosted zeroship** — cloud only
- **Native mobile app** — responsive web only
- **PWA / installable / push notifications / offline mode** — responsive web only; revisit only if real demand surfaces post-launch
- **Multi-region in V1** — single global region; revisit if customers demand
- **Custom build pipelines** — zeroship's build is opinionated by design; if you need a custom build, you're on the wrong platform

---

## Open questions

1. **Mode model — confirm B** (Maker / +Data / +Code, three tiers)?
2. **End-user auth UX** — should creators *configure* providers via a UI panel, or does the AI just *write the code* and the creator stays at the chat level? Hybrid is what I'd default to.
3. **Region / deployment target** — V1 single global, expose multi-region only when needed?
4. **Templates as marketplace** — V1 curated only, creator-publishing deferred to V2+?
5. **GitHub import** — V2+, or sneak into V1?
6. **2FA + GDPR export** — V1.5 (post-launch, pre-monetization)?
7. **Compliance posture for V1** — do we need SOC2 / GDPR copy in V1 marketing?
8. **The 15% revenue share** — when do we tell users about it? At signup? At first earning? On the pricing page?
9. **SRE autonomy default** — does SRE auto-deploy fixes, or always ask? My default: **manual** (always ask) for V1; introduce *low-risk auto* in V1.5 with per-project setting.
10. **PM agent voice in chat** — does PM speak in the chat rail (interrupting Builder's flow) or only inside the plan canvas? My default: only in plan canvas with a notification badge in the top bar; PM responds in chat *only* when @mentioned (`@pm what's next?`).
11. **Skill catalog visibility** — should the full skill list be public-facing (marketing) or behind-the-scenes? My default: public, prominently — it's the most direct answer to "what can this thing actually build?"
12. **Theme application risk** — applying a theme rewrites a lot of code. Is it a normal deploy that can be rolled back? My default: yes, themes apply via the same deploy/rollback pipeline; preview first.
13. **Feature-set dependencies** — if "Comments" needs "Auth", auto-install Auth or block? My default: auto-install with a one-line heads-up in the receipt.
14. **Agent attribution in audit log** — every action labelled (human creator / Builder / PM / SRE / Reviewer)? My default: yes, always.
15. **Domain pack scope** — CRM is far more complex than Todo. Do we ship CRM as one skill, or as a *composition* of capability skills (auth + data + tables + email + roles)? My default: composition. Domain packs are recipes that pull capability skills together, not standalone bodies of code.
16. **Skill / theme / feature-set updates** — when zeroship updates its built-in skill (better Stripe integration ships), do existing apps get the update automatically, opt-in, or never? My default: opt-in with an in-app prompt ("there's an updated payments skill — want me to migrate?").
17. **Critic loop iteration count** — default 3 is my proposal. More = higher quality but slower + more expensive. Acceptable, or do you want a different default? (Note: this trades latency for quality, and creators on free tier might want fewer iterations to save tokens.)
18. **Quality scorecard public visibility** — opt-in badge on the live app, like a Yelp rating? Or keep scorecard private to creators (internal accountability tool)? My default: opt-in public, with the public-showcase listing requiring ≥ B average so the marketplace stays healthy.
19. **Hard gates vs creator override** — a creator who wants to ship a deliberately broken thing (prototyping, demo, art piece) can override soft gates. Should hard gates *ever* be overridable? My default: no — security and "no secrets in client bundle" are non-negotiable. Compile/typecheck/dep-CVE: hard gate, no override.
20. **Scorecard weighting** — UX-completeness might matter more than SEO for a private internal tool, vice versa for a public marketing site. Per-project weighting? Or one global weight that's "good enough"? My default: ship V1 with one global weight; introduce per-project tuning when creators ask.
21. **Critic disagreements with Builder** — both are LLM-driven; they can be wrong differently. What's the tiebreaker? My default: Critic outranks Builder for *blocking* issues (security, bugs); Builder outranks for *style* issues (architecture, naming) so we don't ping-pong forever.

---

## Open scope challenges

- **#84 Cost / token meter** — does Ali want to see this? Could feel intimidating. Maybe show only in +Code tier.
- **#90 Click-to-edit overlay** — high value, technically complex (needs an injectable overlay in the iframe). Worth V1?
- **#168 Multiple custom domains** — V1 single, V2 multi?
- **#181 Deploy history** — if "every AI change auto-deploys", this could grow huge fast. Pruning policy?
- **#225 Stripe Checkout integration for built apps** — does the builder generate Checkout code, or do we provide a `@zeroship/payments` package?
- **#377-380 SRE telemetry pipeline** — collecting per-route latency/errors needs an observability backend. Build into control plane (postgres + materialized views)? Or external (ClickHouse / Vector)? My lean: build into control plane for V1; swap out if scale demands.
- **#394 Auto-fix code generation** — this is *Builder agent run by SRE*, not a separate skill. Same tool surface, different trigger. Need to be sure Builder is robust enough to be trusted unsupervised on a fix.
- **#358 Theme application** — switching theme on a half-built app risks ugly merge edges (e.g. component using old token names). Need a "theme migration" skill in Builder.
- **#365-366 Feature-set integration** — same risk, larger surface. Adding "Auth" mid-project means rewiring routes, adding middleware, generating UI. Builder needs a robust integration playbook, not ad-hoc edits.

---

## Next steps after this is locked

1. Resolve open questions above.
2. Drop / reorder priorities based on your feedback.
3. Promote this to the design spec at `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md`, which will include:
   - All features above with concrete UX / behavior per feature
   - Wireframes / mockups for every primary surface
   - Voice & copy guide
   - Design system (colors, type, motion, components)
   - Information architecture map
   - User flow diagrams (first-run, prompt-to-ship, monetization)
   - Empty / loading / error state patterns
   - Mobile responsive strategy
   - API surface / data model touchpoints
   - Telemetry / analytics events
   - Implementation phasing tied to priorities [0] → [5]
