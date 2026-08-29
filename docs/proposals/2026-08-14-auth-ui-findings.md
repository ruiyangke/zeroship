# Auth UI browser accessibility findings

Status: evidence only. This pass intentionally changes no accessibility behavior.

The initial browser run found the first two defects below. While this harness
work was still in progress, separate concurrent accessibility edits added the
corresponding ARIA markup and were committed as `8fbf5e34f`. This document
preserves the original evidence and also records the final current-tree result;
this pass did not author those accessibility changes.

Measured with real Chromium from Playwright 1.58.2 against the native
`zeroship-auth` release binary. Re-run with:

```bash
nix develop --command bash tests/e2e_auth_ui.sh
```

1. The initial failed-login error had no explicit announcement semantics. The
   banner at `crates/zeroship-auth/src/ui/templates/login.html:6` was a plain
   `div.error`. A real failed login returned HTTP 401 and Chromium reported
   `role=null`, `aria-live=null`, and `alertCount=0`. This DOM evidence does not
   claim how every screen reader handles a full-page 401 navigation. In the
   final current-tree run, the same line had `role="alert"`; Chromium reported
   `role="alert"`, `ariaLive=null`, and `alertCount=1`, and the browser assertion
   passed.

2. The initial failed-login fields were not programmatically tied to the
   error. The email and password inputs, now at
   `crates/zeroship-auth/src/ui/templates/login.html:17` and
   `crates/zeroship-auth/src/ui/templates/login.html:21`, both initially reported
   `ariaInvalid=null`, `ariaDescribedBy=null`, and no description target after
   the same HTTP 401 response. In the final current-tree run, both reported
   `ariaInvalid="true"`, `ariaDescribedBy="form-error"`, an existing target,
   and linkage to the visible error; the browser assertion passed.

3. No accessible-name defect was found in the public forms. The wrapped labels
   and button text at `crates/zeroship-auth/src/ui/templates/login.html:16`,
   `crates/zeroship-auth/src/ui/templates/signup.html:11`, and
   `crates/zeroship-auth/src/ui/templates/forgot.html:13` gave every visible form
   control a non-empty accessible name. Playwright measured 5 of 5 controls on
   `/login` after opening the magic-link disclosure, 4 of 4 on `/signup`, and
   2 of 2 on `/forgot`.

4. No heading-order defect was found. The shared heading at
   `crates/zeroship-auth/src/ui/templates/base.html:12` was the only `h1`; each route's
   page heading at `crates/zeroship-auth/src/ui/templates/login.html:4`,
   `crates/zeroship-auth/src/ui/templates/signup.html:4`, and
   `crates/zeroship-auth/src/ui/templates/forgot.html:4` was an `h2`. Chromium measured
   exactly one `h1` and heading levels `[1, 2]` on all three pages.

5. No focus-visibility defect was found in Chromium. The controls styled at
   `crates/zeroship-auth/static/style.css:7`, `crates/zeroship-auth/static/style.css:8`, and
   `crates/zeroship-auth/static/style.css:10`, plus the summary at
   `crates/zeroship-auth/static/style.css:75`, retain the browser focus ring. Keyboard
   traversal measured a `1px` `auto` outline and `:focus-visible=true` on all
   8 unique interactive elements on `/login`, all 4 on `/signup`, and all 3 on
   `/forgot`. This is evidence for Chromium's current user-agent focus style,
   not a cross-browser claim.
