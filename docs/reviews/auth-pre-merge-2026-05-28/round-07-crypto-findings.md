# Round 7 - Crypto hygiene: findings

Total: 6 findings (0 critical, 1 high, 3 medium, 2 low).

Scope note: `crates/auth/src/store/env_store.rs` does not exist in this worktree. The encrypted app-env secret store is `crates/control/src/env_store.rs`, backed by `crates/core/src/crypto.rs`.

## CRITICAL

(none)

## HIGH

### H1. App secret ciphertexts are not bound to `app_id` or `key_name`
**File:** `crates/core/src/crypto.rs:70`, `crates/control/src/env_store.rs:269`, `crates/control/src/env_store.rs:353`, `crates/control/src/env_store.rs:497`

**Severity rationale:** `crypto::encrypt` calls AES-256-GCM with only plaintext (`encrypt(&nonce, plaintext)`) and no associated data. `EnvStore` stores only `nonce || ciphertext || tag` and decrypts the blob without passing the row context. Because every app uses the same derived control-plane key, any actor with a DB write primitive but without the master key can transplant one row's ciphertext into another `(app_id, key_name)` row and the target app will decrypt it successfully. That turns an app-env table write bug or compromised DB role into cross-app secret exfiltration through the normal worker env endpoint.

**Reproducer:**
1. Create app A with secret `STRIPE_KEY=sk_live_victim`.
2. Create app B controlled by the attacker.
3. In Postgres, copy A's ciphertext into B:
   ```sql
   INSERT INTO app_secrets(app_id, key_name, ciphertext)
   SELECT '<app-b-uuid>', 'COPIED_SECRET', ciphertext
   FROM app_secrets
   WHERE app_id = '<app-a-uuid>' AND key_name = 'STRIPE_KEY'
   ON CONFLICT (app_id, key_name) DO UPDATE
     SET ciphertext = EXCLUDED.ciphertext;
   ```
4. Fetch app B's worker env. `merged_env_for_worker` decrypts `COPIED_SECRET` with the same key and returns `sk_live_victim`.

**Suggested fix:** Change the AEAD API to accept AAD and bind at least `app_id`, `key_name`, and a purpose/version string, for example `b"zs:control:app_secret:v1\0" || app_id || b"\0" || key_name`. Since this is pre-launch, re-encrypt the table in the same change and update tests to assert cross-app and cross-key ciphertext swaps fail to decrypt. A per-app HKDF key derived from the master key and `app_id` would also reduce blast radius, but AAD is still needed to bind `key_name`.

## MEDIUM

### M1. Control master key derivation is a single SHA-256 with no entropy check
**File:** `crates/core/src/crypto.rs:51`, `crates/control/src/env_store.rs:127`, `crates/control/src/env_store.rs:137`

**Severity rationale:** Production only rejects an empty `MASTER_KEY`; any non-empty string is accepted and turned into the AES key with `SHA256("zeroship-secret-key-v1" || master)`. There is no memory-hard KDF, no salt, and no minimum entropy/format validation. If an operator uses a human password or short shared secret, a DB dump of `app_secrets.ciphertext` becomes an offline password-cracking target. Because the same derived key protects every app secret, each guessed master key is amortized across the whole table.

**Reproducer:**
1. Start control with `MASTER_KEY=correct horse battery staple` or another dictionary-style string.
2. Obtain any `app_secrets.ciphertext` row from a database backup.
3. Offline, iterate a wordlist through the current `derive_key` function and AES-GCM decrypt attempts. A successful decrypt is easy to recognize from valid UTF-8 or known secret prefixes such as `sk_`, `ghp_`, or JSON service-account data.

**Suggested fix:** Pick one contract and enforce it at boot. The simplest pre-launch shape is `MASTER_KEY` must decode to 32 random bytes (base64url or hex) and is then fed into HKDF for domain separation. If password-style master keys must be supported, use Argon2id or PBKDF2 with a persisted deployment salt and explicit parameters. Add startup tests that reject short/default-looking master keys in non-dev mode.

### M2. JWK old-key retirement never runs under the default rotation cadence
**File:** `crates/auth/src/cron/jwk_rotation.rs:114`, `crates/auth/src/cron/jwk_rotation.rs:177`, `crates/auth/src/cron/jwk_rotation.rs:189`, `crates/auth/src/cron/jwk_rotation.rs:213`

**Severity rationale:** The cron uses one `auth.cron_state.last_rotated_at` timestamp for both rotation due-ness and retirement age. With defaults (`rotation_days = 90`, `retain_days = 31`), every normal tick at day 90 sees `days_since = 90`, skips retirement because `90 < 121`, rotates, and records `last_rotated_at = NOW()`. The next cycle repeats with `days_since = 90`. As long as cron is running normally, `days_since` never reaches `rotation_days + retain_days`, so old signing keys accumulate indefinitely. A retired or compromised old Hydra private key therefore remains published and accepted far beyond the intended 31-day overlap.

**Reproducer:**
1. Seed a key set and set `auth.cron_state.last_rotated_at = NOW() - INTERVAL '90 days'`.
2. Run `tick_once_for_test(..., 90, 31)`. `retire_stale_keys` skips deletion, `rotate_set_if_due` creates new keys, and `record_rotated_now` resets the timestamp.
3. Repeat step 1 from the new timestamp after another simulated 90 days. The previous generations are still present because retirement again sees only 90 days since the latest rotation.

**Suggested fix:** Track retirement state separately from rotation state, or retire by key generation/creation time rather than the latest rotation timestamp. For this list-based Hydra store, the pragmatic pre-launch fix is: after a successful rotation, retain only the current generation plus the previous generation for each algorithm once the previous generation is older than `retain_days`. Add a regression test that runs three simulated 90-day rotations and asserts generation 0 is removed.

### M3. Gateway OIDC stash HMAC key falls back to a public default outside dev mode
**File:** `crates/gateway/src/main.rs:48`, `crates/gateway/src/main.rs:54`, `crates/gateway/src/main.rs:189`, `crates/gateway/src/oidc_rp.rs:380`, `crates/gateway/src/oidc_rp.rs:392`

**Severity rationale:** The gateway defaults `STASH_SIGNING_KEY` to `dev-stash-key-please-rotate` and never rejects it when `INSECURE_DEV=false`. That key signs the per-app `__Host-zs_oidc_stash` containing OAuth `state`, PKCE verifier, OIDC nonce, original path, and redirect URI. Control and auth have production guards for equivalent stash keys; gateway does not. In a misconfigured deployment, stash integrity depends on a known public string, so any stash-cookie tamper/injection primitive becomes a valid signed stash instead of failing HMAC verification.

**Reproducer:**
1. Start `zeroship-gate` without `STASH_SIGNING_KEY` and without `INSECURE_DEV=true`.
2. Observe boot continues and constructs `OidcRp` with `dev-stash-key-please-rotate`.
3. Generate `base64url(json).base64url(HMAC_SHA256("dev-stash-key-please-rotate", base64url(json)))` for a forged stash body. `Stash::decode` accepts it because the production process is using the public default.

**Suggested fix:** Mirror `auth::config::validate_stash_key`: in non-dev mode, reject missing/default stash keys and require at least 32 bytes of operator-supplied entropy. Prefer separate env vars for control and gateway stash keys so compromise of one RP's transient state does not automatically authenticate the other's state cookies.

## LOW

### L1. SES-SNS webhook verification is pinned to legacy RSA-SHA1 signatures
**File:** `crates/auth/src/ui/webhooks.rs:200`, `crates/auth/src/ui/webhooks.rs:221`, `crates/auth/src/mailer/sns.rs:30`, `crates/auth/src/mailer/sns.rs:237`

**Severity rationale:** The SNS webhook handler rejects every `SignatureVersion` except `1`, and the verifier uses `RSA_PKCS1_1024_8192_SHA1_FOR_LEGACY_USE_ONLY`. This is compatible with legacy SNS deliveries, but it keeps SHA-1 in an inbound webhook authenticity boundary even though SNS supports SignatureVersion 2 with SHA-256. It also prevents operators from hardening the SNS topic to v2 without breaking bounces/complaints.

**Reproducer:**
1. Configure an SNS test notification using `SignatureVersion = "2"`.
2. POST it to `/webhooks/ses-sns`.
3. The handler returns 400 at the version check, so the only accepted signature scheme remains RSA-SHA1.

**Suggested fix:** Add SignatureVersion 2 support using RSA-SHA256 and make v2 the default accepted scheme for new deployments. If v1 must remain during rollout, gate it behind an explicit compatibility flag and document the planned removal.

### L2. Sandbox bearer-token checks still use length-short-circuit raw compares
**File:** `crates/sandbox/src/auth.rs:43`, `crates/sandbox/src/preview_ws.rs:543`

**Severity rationale:** The sandbox HTTP and preview-WebSocket bearer checks return before `ct_eq` when the presented token length differs from the configured token length. That avoids prefix leaks but still exposes the expected token length over timing. The admin API already moved to a stronger pattern by hashing both presented and expected values and constant-time comparing fixed-size digests; these two paths have not been brought along.

**Reproducer:**
1. Configure a non-empty `SANDBOX_TOKEN`.
2. Send repeated unauthenticated HTTP or preview-WS requests with `Authorization: Bearer <N bytes>` for varying `N`.
3. Wrong-length requests return before the `ct_eq` call, while equal-length wrong tokens execute the full comparison. With enough local samples, the configured token length is distinguishable.

**Suggested fix:** Reuse the admin API's digest-compare helper shape: SHA-256 both presented and expected bytes, then `ct_eq` the 32-byte digests. Optionally cap the accepted `Authorization` header length before hashing to bound attacker-controlled work.

## Areas reviewed and clean

- CSRF tokens: 128-bit random token generation, equal-length XOR compare, and `__Host-` production cookie naming are sound for this use.
- Stripe webhook HMAC: timestamp window, v1 count cap, and no early exit across multiple signatures are in place.
- OAuth stash and pending-link HMACs: MAC verification runs before JSON parse and uses constant-time byte accumulation for valid-length MACs.
- Password hashing: Argon2id v0x13 with 19 MiB memory, 2 iterations, p=1, `OsRng` salts, and dummy-hash padding for missing users.
- High-entropy email tokens: magic link, password reset, and email verification tokens use 32 random bytes and store SHA-256 digests only. `rand::thread_rng()` is a CSPRNG in the pinned `rand` line; switching these to `OsRng` would improve explicitness but was not counted as a finding.
- PAT and wrapper JWTs: `alg` is pinned to EdDSA in `Validation`, `typ` is checked, `kid` is pre-checked, and issuer/audience validation is configured.
- DPoP proof verification: symmetric/`none` algorithms are excluded, `typ`, `htm`, `htu`, `iat`, and `ath` are checked, and JWK thumbprints use required members only.
