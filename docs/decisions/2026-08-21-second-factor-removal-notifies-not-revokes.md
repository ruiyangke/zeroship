# ADR - removing a second factor notifies the account holder and revokes nothing

- **Date:** 2026-08-21
- **Status:** Accepted
- **References:** `crates/zeroship-auth/src/ui/totp.rs`,
  `crates/zeroship-auth/tests/totp_removal_notice_test.rs`,
  `crates/zeroship-mailer/src/templates/second_factor_removed.{html,txt}`,
  `crates/zeroship-auth/src/identity/password_reset.rs`,
  `crates/zeroship-auth/src/ui/sessions.rs`

## Context

`/me/2fa/disable` sat next to three routes that all tear something down and was
the only one of the four that produced no signal beyond an audit row.
`/reset` writes a `(client_id, sub)` family marker into
`zeroship.token_revocations`, revokes every `zeroship.app_session_anchors` row,
bumps `users.credential_version` and deletes the session rows. `/logout` and
`/me/sessions/{id}/revoke` emit OIDC back-channel logout. `/me/2fa/disable` did
none of that, and neither did `/me/2fa/enroll`, which reaches the same state by
resetting a confirmed credential to pending.

The argument for adding a teardown is real and worth stating plainly: an actor
on a stolen live session can remove the victim's second factor, every session
including theirs keeps working, and the account is permanently easier to
re-enter afterwards.

### What the handler already requires

It does not fall to a session cookie alone. Both routes call `verify_reauth`,
which accepts EITHER a current TOTP code decrypted against the stored secret OR
the account password verified with Argon2 on `spawn_blocking`. Absent or wrong,
the request is refused 401 `reauth_required` and audited
(`totp_disable_failed` / `totp_enroll_refused`). CSRF adds nothing against this
adversary and is not meant to: `csrf::matches` is a bare double-submit byte
equality with no signature and no binding to the session, so anyone holding the
cookie sets both halves.

## Decision

Neither route revokes anything. Both send a second-factor-removed notice to the
registered address whose call to action is a password reset.

## Consequences and reasoning

**A teardown would return no capability to the defender.** The proof the route
demands is the same material that signs the actor back in once 2FA is off. An
actor who passed the password arm holds the password, and `/login` now takes the
password alone; an actor who passed the code arm holds the authenticator. Either
way, revoking every session evicts the account holder and costs the actor one
redirect.

**A teardown could not stop at `disable`.** `/me/2fa/enroll` over a confirmed
credential sets `confirmed_at = NULL`, so `is_enabled` goes false and `/login`
stops challenging - the same unprotected state, behind the same proof. A
teardown on `disable` alone is routed around by enrolling. A teardown on both
signs every device out on every authenticator rotation, which is the routine
new-phone flow, not a security event.

**Detection is the control that helps, and the remedy already exists.** The
account holder finding out is what converts this from a silent permanent
downgrade into a recoverable one, and the action to recover is a password reset,
which does revoke everything. The notice therefore links to `/forgot`, not to a
session list.

## What comparable systems do

Surveyed 2026-08-21. No system in this set revokes other sessions when a factor
is removed.

| System | Re-auth to remove? | Other sessions killed? | Notification? |
| --- | --- | --- | --- |
| GitLab | yes, current password | no | yes, `disabled_two_factor_otp_email` |
| Ory Kratos | yes, privileged session | no | not found |
| Supabase GoTrue | yes, AAL2 | no, only that factor's sessions drop to AAL1 | not found |
| Auth0 | yes, `mfa`-audience token | no | not documented |
| Keycloak | yes, `acr`/LoA on the token | no evidence | yes, on `REMOVE_TOTP` |
| Okta | not documented | not documented | yes, default-on authenticator-reset email |
| django-allauth | yes, `reauthentication_required` | no | yes, `mfa/email/totp_deactivated` |
| Microsoft Entra ID | n/a | no | not documented |
| AWS IAM | yes, via `aws:MultiFactorAuthPresent` | not documented | not documented |
| GitHub | not documented | not documented | security-log entry only |

GitLab draws the same line we do and draws it sharply:
`destroy_all_but_current_user_session!` is called when TOTP is ENABLED and not
when it is destroyed. Microsoft Entra ID makes that cut normative - its five
continuous-access critical events include MFA being *enabled* and exclude method
removal.

Standards land the same way. OWASP's Multifactor Authentication Cheat Sheet asks
for re-authentication ("do not rely solely on the active session, as it may be
hijacked") and out-of-band notification "whenever an MFA factor is changed", and
says nothing about other sessions. OWASP's Session Management Cheat Sheet lists
only password change as a termination trigger. NIST SP 800-63B Rev 4 makes
notification on invalidation a SHOULD (Sec. 4.5) and on binding a new
authenticator a SHALL (Sec. 4.1.2.1).

ASVS 5.0 7.4.3 (L2) is the one source connecting factor removal to sessions, and
it asks that the application "gives the option to terminate all other active
sessions", not that it terminate them. `/me/sessions` and
`/me/sessions/{id}/revoke` are that option and they emit back-channel logout.

One divergence worth keeping: Kratos issue 2450 reports that unlinking TOTP
there accepts any `totp_code`, empty string included, leaving the
privileged-session window as the only real gate. `verify_reauth` verifies the
proof it asks for.

## What this does not cover

The notice is best-effort at the transport, exactly like `/forgot`'s reset mail:
a suppressed address or a provider outage is logged and swallowed rather than
turning a completed credential change into a 5xx that invites a retry of a write
that already landed. An account whose registered address is compromised gets no
signal from this at all; that case is addressed by the re-auth gate, not by the
notice.

## Revisit if

- The re-auth proof is ever widened - a backup code, a magic link, a
  recently-authenticated grace window - because the argument above rests
  entirely on the proof being sufficient for a fresh `/login`.
- A second notification address lands (NIST Rev 4 Sec. 4.6 asks for at least
  two), which would make the notice meaningfully harder to suppress.
