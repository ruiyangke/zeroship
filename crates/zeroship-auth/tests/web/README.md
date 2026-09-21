# Auth UI browser gate (`crates/zeroship-auth/tests/web/`)

Playwright specs that drive **real Chromium** against the **real
`zeroship-auth` binary**, booted natively on a loopback port over a real
migrated Postgres. This is the only tier that can see what a browser actually
does with an auth page: which CSP directives fire, what the accessibility tree
exposes, whether a declaration was applied or silently dropped.

## Why this tier exists

The Rust suites in `crates/zeroship-auth/tests/` drive handlers over HTTP and assert on
response bytes. That is the right level for flow logic, and it is blind by
construction to everything the browser decides afterwards. Two defects found on
the day this was written are both invisible to a response-body assertion:

- `login.html` carried an inline `style=` attribute while every auth response
  sends `style-src 'self'`. The bytes were correct and the browser **blocked**
  the declaration:
  `Applying inline style violates ... 'style-src 'self''. The action has been
  blocked.` (`effectiveDirective=style-src-attr`). The "Forgot password?" link
  lost its intended layout on the most-used page in the product.
- A failed sign-in rendered its error as a bare `generic` node with both inputs
  plain `textbox`, so a screen reader announced nothing at all. The HTML was
  present and correct; the *semantics* were missing.

## What it covers

| Spec | Asserts (in a real browser) |
| --- | --- |
| `csp.spec.ts` | `/login` enforces `style-src 'self'` with neither `unsafe-inline` nor `unsafe-hashes`; ZERO style violations via both `page.on("console")` and the document `securitypolicyviolation` event; the forgot-link row computes `text-align: right` (so the fix is a real layout, not a silenced console). |
| `a11y.spec.ts` | A real failed login (HTTP 401) exposes `role="alert"` on the error and `aria-invalid` + `aria-describedby` on both fields, with the description target present; every visible control on `/login`, `/signup`, `/forgot` has an accessible name; exactly one `h1` per page; every keyboard target matches `:focus-visible` with a non-transparent outline. |
| `flows.spec.ts` | A verified user enrolls RFC 6238 TOTP through the authenticated endpoints, proves the clean challenge has no error semantics, proves a rejected code exposes an addressable alert, and completes login with a current code. A separate verified user reaches real OIDC consent for a registered per-app client, sees the app scope label and block-level description, sees visibly distinct Allow/Deny actions, and Deny returns `access_denied` to the RP. |
| `journey.spec.ts` | One browser context signs up, reads and redeems the verification link from the live auth log, signs in, checks `/me`, signs out, proves `/me` is gated again, compares wrong-password and missing-email failures byte-for-byte, and rejects verification-link replay. |

## How to run

```bash
nix develop --command cargo test -p zeroship-auth --test main -- --ignored auth_ui
```

The case is `#[ignore]`d so a browser never becomes a prerequisite of
`cargo test -p zeroship-auth`. It is self-contained through the crate's own
fixtures: an owned PostgreSQL container migrated from the real corpus, the
production auth router served in-process, and a mailer that appends the
`=== MAIL ===` block to the log these specs read. A run that could not happen
fails loudly - provisioning panics rather than reporting green.

It does NOT cover the `zeroship-auth` binary's own startup (argument parsing,
secret-file loading, the readiness gate). Those are process-level contracts owned
by `config_env_tier` and `check_config_smtp_test`.

You must be inside the nix env: the runner is nix `playwright` 1.58.2 with
`PLAYWRIGHT_BROWSERS_PATH`, and `scripts/link-playwright.sh` points the local
`@playwright/test` at the runner's own copy. npm-downloaded browsers cannot link
their libraries on NixOS. Do not install a different Playwright version - version
skew breaks browser launch.

## Not in CI, deliberately

`ci.yml` does not run this, exactly as it does not run `tests/e2e_browser/`:
both need a nix env, real browsers and a live multi-process stack. Run it by
hand when touching auth UI. The normal Rust suite exercises rendered login
markup and response headers. Template source scans are retired; browser checks
cover computed styles, interaction and accessibility-tree behavior.
