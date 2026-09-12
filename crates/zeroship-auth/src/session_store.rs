//! `zeroship.sessions` + `zeroship.grants`: the session object, and the only
//! place in this service a credential may be minted from.
//!
//! # MINT-READS-ROW
//!
//! No credential is issued except from a validating read of a session row, and
//! that read is the same statement that enforces liveness, expiry, the
//! session's grant status and the person's credential epoch. Each validating
//! operation returns a [`ValidatedSession`]:
//!
//! - [`create`] - `INSERT ... SELECT` over `zeroship.users` joined to
//!   `zeroship.grants`. The row it writes is the row it returns, so the first
//!   mint of a session is made from a row that exists and passed the same
//!   predicates every later mint will face.
//! - [`rotate`] - the validating `UPDATE ... RETURNING` that advances the
//!   secret and slides the idle window.
//! - [`replay`] - the validating `UPDATE ... RETURNING` that consumes the
//!   single-use idempotent record a lost rotation earns.
//!
//! [`ValidatedSession`] has private fields and no public constructor, so a mint
//! path that skips the read does not compile rather than failing a check
//! nothing reaches. Store integration tests exercise the validating operations
//! against `PostgreSQL`, including accepted controls alongside refused mints.
//!
//! # The refresh family is ONE ROW
//!
//! `zeroship.oauth_refresh_tokens` stored a row per token and linked them by
//! `refresh_family_id`. The family is now the session: the secret rotates IN
//! PLACE, the superseded hash lives in `prev_secret_hash`, and killing the
//! family is a write to the row that mints rather than to a sibling table a
//! reader has to remember to consult. The algorithm is otherwise the one that
//! was there - rotate on use, serve one idempotent replay of a lost response,
//! and treat any other presentation of a superseded secret as reuse.
//!
//! **The reuse-detection HORIZON narrows, and this is the one behavioural
//! difference worth knowing.** The old shape retained every historical token
//! row until a sweep deleted it, so presenting a secret from any past
//! generation was detected as reuse. One row carries one superseded slot, so
//! detection now covers the IMMEDIATELY preceding secret - which is the
//! generation an interception attack actually holds - and a secret two or more
//! rotations old matches nothing and is refused as unknown rather than killing
//! the session. The slot is kept for the life of the session, NOT cleared when
//! the idempotency response expires: those are two different deadlines, and
//! collapsing them would shrink the horizon to the replay window for no gain.
//!
//! # What this module does NOT decide
//!
//! The audience is `platform` or `app`, and the app arm is keyed by the OAuth
//! `client_id` - today's scope, unchanged. The subject stored on the grant is
//! today's derivation: the person's own id for the platform audience, the
//! pairwise subject over the client's sector identifier otherwise. Moving
//! either to the project is a later step with its own migration ordering.

use std::path::Path;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use compio_postgres::{GenericClient, Row};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroship_core::auth::hmac_sha256;
use zeroship_core::crypto;
use zeroship_core::device_grant::PLATFORM_CLI_CLIENT_ID;
use zeroship_core::typed_id;
use zeroship_core::UserId;

use crate::error::{AuthError, Result};

/// Bytes of CSPRNG entropy behind a session secret.
const SECRET_BYTES: usize = 32;
/// The wire prefix of a session secret. It is what an OAuth client receives as
/// its `refresh_token`, so the spelling is load-bearing on the wire.
const SECRET_PREFIX: &str = "zrt_";
/// AAD domain separator for the sealed idempotent response. Bound to the
/// superseded hash and the session id, so a record sealed for one session
/// cannot open against another.
const IDEM_AAD_PREFIX: &[u8] = b"zs:auth:refresh_idem:v1\0";

/// Proof that a session row was read and validated by the statement that
/// produced this value.
///
/// Private fields, no public constructor, no `Clone`, no `Default` and no
/// `From`. The only way to hold one is to have called [`create`], [`rotate`]
/// or [`replay`] and had the row pass. Adding a public constructor here would
/// hand back exactly the property the type exists to provide: that a mint
/// which skipped the read cannot be written.
#[derive(Debug)]
pub struct ValidatedSession {
    session_id: String,
    person_id: UserId,
    grant_id: String,
    epoch: i64,
}

impl ValidatedSession {
    /// The session the credential is being minted for.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The person the session belongs to.
    #[must_use]
    pub fn person_id(&self) -> &UserId {
        &self.person_id
    }

    /// The grant the session hangs off.
    #[must_use]
    pub fn grant_id(&self) -> &str {
        &self.grant_id
    }

    /// The session epoch the validating read observed.
    #[must_use]
    pub fn epoch(&self) -> i64 {
        self.epoch
    }
}

/// The unit a subject, a grant and a session are scoped to.
///
/// `App` carries the OAuth client id because that is what the scope is TODAY -
/// the same key `zeroship.oauth_grants` and `zeroship.app_user_identities` use.
/// The redesign's end state is the project, and moving it is a separate step
/// that adds and collates the column before keying on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Audience {
    Platform,
    App { client_id: String },
}

impl Audience {
    /// The `audience_kind` discriminant as the column stores it.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Platform => "platform",
            Self::App { .. } => "app",
        }
    }

    /// The client this audience names, or `None` for the platform audience.
    #[must_use]
    pub fn client_id(&self) -> Option<&str> {
        match self {
            Self::Platform => None,
            Self::App { client_id } => Some(client_id.as_str()),
        }
    }
}

/// What kind of thing holds the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Browser,
    Cli,
    DevicePending,
}

impl SessionKind {
    /// The `kind` discriminant as the column stores it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Browser => "browser",
            Self::Cli => "cli",
            Self::DevicePending => "device_pending",
        }
    }
}

/// A session row joined to its grant. Every field a mint needs is here, so a
/// mint never has to go back to the database for something the validating read
/// could have returned.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub person_id: UserId,
    pub audience_kind: String,
    pub client_id: Option<String>,
    pub grant_id: String,
    pub kind: String,
    pub epoch: i64,
    pub credential_epoch: i64,
    pub scopes: Vec<String>,
    /// The grant's scope set: the ceiling a rotation may narrow within and
    /// never widen past. This is what `family_granted_scopes` carried.
    pub grant_scopes: Vec<String>,
    /// The subject this person presents to this audience.
    pub subject: String,
    pub created_at: DateTime<Utc>,
    pub rotated_at: Option<DateTime<Utc>>,
    pub idle_expires_at: DateTime<Utc>,
    pub absolute_expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub idem_response_enc: Option<Vec<u8>>,
    pub idem_expires_at: Option<DateTime<Utc>>,
    pub secret_key_version: Option<i16>,
    pub prev_secret_key_version: Option<i16>,
    /// The stored HMACs, never the secrets. They are here so a caller holding
    /// the row under a lock can re-decide which slot a presented secret matches
    /// - see [`SessionRow::slot_for`].
    secret_hash: Option<Vec<u8>>,
    prev_secret_hash: Option<Vec<u8>>,
}

impl SessionRow {
    /// Which slot the presented secret matches ON THIS ROW, as the row stands
    /// now.
    ///
    /// **The answer [`peek`] gave is advisory and MUST be re-derived here.**
    /// `peek` runs before the transaction and before any lock, so a concurrent
    /// rotation can commit between the two: the caller's secret was the current
    /// one when it was resolved and is the SUPERSEDED one by the time the lock
    /// is granted. Acting on the stale verdict rotates against a hash that has
    /// moved, updates nothing, and refuses a request that should have been
    /// served the idempotent replay - which is exactly what
    /// `concurrent_refresh_same_token_serializes_to_one_successor_without_family_kill`
    /// caught. The old shape did not have this hazard, because it re-read the
    /// row by token hash and asked `rotated_at`; one row per family means the
    /// question has to be asked of the slots instead.
    #[must_use]
    pub fn slot_for(&self, presented: &PeekedSession) -> Option<SecretSlot> {
        if self.secret_hash.as_deref() == Some(presented.hash.as_slice()) {
            return Some(SecretSlot::Current);
        }
        if self.prev_secret_hash.as_deref() == Some(presented.hash.as_slice()) {
            return Some(SecretSlot::Superseded);
        }
        None
    }

    fn proof(&self) -> ValidatedSession {
        ValidatedSession {
            session_id: self.id.clone(),
            person_id: self.person_id.clone(),
            grant_id: self.grant_id.clone(),
            epoch: self.epoch,
        }
    }
}

/// Which of a row's two secret slots a presented credential matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretSlot {
    /// The live secret: this presentation rotates.
    Current,
    /// The superseded secret: this presentation is a replay or reuse.
    Superseded,
}

/// The non-locking lookup a caller does before it opens a transaction, so
/// client authentication can be settled before a row is locked.
#[derive(Debug, Clone)]
pub struct PeekedSession {
    pub session_id: String,
    pub person_id: UserId,
    pub client_id: Option<String>,
    slot: SecretSlot,
    hash: Vec<u8>,
    version: i16,
}

impl PeekedSession {
    /// Which slot the presented secret matched.
    #[must_use]
    pub fn slot(&self) -> SecretSlot {
        self.slot
    }
}

/// A rotated session and the credential it hands back.
///
/// [`rotate`] returns `Ok(None)` when the validating read refuses, which is the
/// same shape and the same meaning [`create`] uses. Two functions whose refusal
/// means the same thing say it the same way; an enum with one large variant and
/// one empty one said it differently for no gain.
#[derive(Debug)]
pub struct RotatedSession {
    pub row: SessionRow,
    /// The successor secret to hand back.
    pub secret: String,
    pub proof: ValidatedSession,
}

/// The response a lost rotation is allowed to replay exactly once.
#[derive(Debug, Serialize, Deserialize)]
pub struct CachedResponse {
    pub refresh_token: String,
    pub scope: String,
}

// ---------------------------------------------------------------------------
// Secret keyring
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SecretHashKey {
    version: i16,
    key: Vec<u8>,
}

/// A hash and the keyring version that produced it.
#[derive(Debug, Clone)]
pub struct SecretHash {
    pub version: i16,
    pub hash: Vec<u8>,
}

/// The versioned HMAC keyring the session secret is stored under, plus the AEAD
/// key that seals an idempotent response.
///
/// This is the refresh family's keyring, moved with the family. It is still
/// configured under the same two settings: the credential on the wire is still
/// an OAuth `refresh_token`, so those names stay accurate.
#[derive(Debug, Clone)]
pub struct SessionSecretKeys {
    active: SecretHashKey,
    verify: Vec<SecretHashKey>,
    idem_key: [u8; 32],
}

impl SessionSecretKeys {
    /// Load the keyring from the hash-keyring file and the idempotency key
    /// file.
    ///
    /// # Errors
    ///
    /// A message naming the setting when either file is missing, unreadable,
    /// group- or world-accessible, or yields no usable key.
    pub fn from_files(hash_file: &Path, idem_file: &Path) -> std::result::Result<Self, String> {
        let mut verify = load_hash_keyring(hash_file)?;
        verify.sort_by(|a, b| b.version.cmp(&a.version));
        let Some(active) = verify.first().cloned() else {
            return Err(format!(
                "REFRESH_HASH_KEY_FILE {} yielded no keys",
                hash_file.display()
            ));
        };
        let idem_secret = read_secret_file(idem_file, "REFRESH_IDEM_KEY_FILE")?;
        let idem_key = crypto::derive_key(&String::from_utf8_lossy(&idem_secret));
        Ok(Self {
            active,
            verify,
            idem_key,
        })
    }

    /// Hash a secret under the active key.
    #[must_use]
    pub fn active_hash(&self, raw: &str) -> SecretHash {
        SecretHash {
            version: self.active.version,
            hash: hmac_sha256(&self.active.key, raw.as_bytes()).to_vec(),
        }
    }

    /// Every candidate hash for a presented secret, newest key first.
    #[must_use]
    pub fn hashes_newest_first(&self, raw: &str) -> Vec<SecretHash> {
        self.verify
            .iter()
            .map(|key| SecretHash {
                version: key.version,
                hash: hmac_sha256(&key.key, raw.as_bytes()).to_vec(),
            })
            .collect()
    }

    /// Seal the response a lost rotation may replay once.
    ///
    /// # Errors
    ///
    /// `AuthError::Internal` when the body will not encode or seal.
    pub fn seal_cached_response(
        &self,
        superseded_hash: &[u8],
        session_id: &str,
        body: &CachedResponse,
    ) -> Result<Vec<u8>> {
        let aad = idem_aad(superseded_hash, session_id);
        let plain = serde_json::to_vec(body)
            .map_err(|err| AuthError::Internal(format!("seal idempotency cache: {err}")))?;
        crypto::encrypt(&self.idem_key, &aad, &plain)
            .map_err(|err| AuthError::Internal(format!("seal idempotency cache: {err}")))
    }

    /// Open a sealed idempotent response.
    ///
    /// # Errors
    ///
    /// `AuthError::Internal` when the record will not open. [`replay`] turns
    /// that into the reuse verdict rather than into a server fault; see its
    /// documentation for why the direction matters.
    pub fn open_cached_response(
        &self,
        superseded_hash: &[u8],
        session_id: &str,
        enc: &[u8],
    ) -> Result<CachedResponse> {
        let aad = idem_aad(superseded_hash, session_id);
        let plain = crypto::decrypt(&self.idem_key, &aad, enc)
            .map_err(|err| AuthError::Internal(format!("open idempotency cache: {err}")))?;
        serde_json::from_slice(&plain)
            .map_err(|err| AuthError::Internal(format!("decode idempotency cache: {err}")))
    }
}

fn idem_aad(superseded_hash: &[u8], session_id: &str) -> Vec<u8> {
    let mut aad =
        Vec::with_capacity(IDEM_AAD_PREFIX.len() + superseded_hash.len() + session_id.len() + 1);
    aad.extend_from_slice(IDEM_AAD_PREFIX);
    aad.extend_from_slice(superseded_hash);
    aad.push(0);
    aad.extend_from_slice(session_id.as_bytes());
    aad
}

fn load_hash_keyring(path: &Path) -> std::result::Result<Vec<SecretHashKey>, String> {
    let raw = read_secret_file(path, "REFRESH_HASH_KEY_FILE")?;
    let text = std::str::from_utf8(&raw).map_err(|err| {
        format!(
            "REFRESH_HASH_KEY_FILE {} must be UTF-8 version:key lines: {err}",
            path.display()
        )
    })?;
    let mut keys = Vec::new();
    for (idx, line) in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let Some((version, secret)) = line.split_once(':') else {
            return Err(format!(
                "REFRESH_HASH_KEY_FILE {} line {} must be version:hex-or-base64url-key",
                path.display(),
                idx + 1
            ));
        };
        let version: i16 = version.trim().parse().map_err(|_| {
            format!(
                "REFRESH_HASH_KEY_FILE {} line {} has invalid version {:?}",
                path.display(),
                idx + 1,
                version
            )
        })?;
        let key = decode_key_material(secret.trim()).ok_or_else(|| {
            format!(
                "REFRESH_HASH_KEY_FILE {} line {} has unparseable key material",
                path.display(),
                idx + 1
            )
        })?;
        if key.len() < 32 {
            return Err(format!(
                "REFRESH_HASH_KEY_FILE {} line {} key for version {} is {} bytes; require at least 32",
                path.display(),
                idx + 1,
                version,
                key.len()
            ));
        }
        keys.push(SecretHashKey { version, key });
    }
    if keys.is_empty() {
        return Err(format!(
            "REFRESH_HASH_KEY_FILE {} yielded no keys",
            path.display()
        ));
    }
    Ok(keys)
}

fn decode_key_material(value: &str) -> Option<Vec<u8>> {
    hex::decode(value)
        .ok()
        .or_else(|| URL_SAFE_NO_PAD.decode(value).ok())
        .filter(|bytes| !bytes.is_empty())
}

fn read_secret_file(path: &Path, label: &str) -> std::result::Result<Vec<u8>, String> {
    let bytes =
        std::fs::read(path).map_err(|err| format!("read {label} {}: {err}", path.display()))?;
    reject_insecure_permissions(path, label)?;
    if bytes.is_empty() {
        return Err(format!("{label} {} is empty", path.display()));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn reject_insecure_permissions(path: &Path, label: &str) -> std::result::Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = path
        .metadata()
        .map_err(|err| format!("stat {label} {}: {err}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "{label} {} has insecure permissions {mode:o}; require owner-only permissions",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_insecure_permissions(_path: &Path, _label: &str) -> std::result::Result<(), String> {
    Ok(())
}

/// Mint a fresh session secret. Opaque, CSPRNG, never derived from anything a
/// caller supplies.
#[must_use]
pub fn generate_secret() -> String {
    let mut bytes = [0u8; SECRET_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!("{SECRET_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

// ---------------------------------------------------------------------------
// Grants
// ---------------------------------------------------------------------------

/// Create or advance the (person, audience) grant, returning its id.
///
/// The subject is written once and never rewritten. An `ON CONFLICT` that
/// overwrote it would let a re-derivation under a changed salt silently
/// re-identify a returning person to the same audience under a new subject,
/// which is the one thing a pairwise subject exists to prevent. Scopes and the
/// relay address ARE the consent, so those advance.
///
/// # Errors
///
/// `AuthError::Db` when the upsert fails.
pub async fn upsert_grant(
    db: &(impl GenericClient + ?Sized),
    person_id: &UserId,
    audience: &Audience,
    subject: &str,
    scopes: &[String],
    relay_email: Option<&str>,
) -> Result<String> {
    let id = typed_id::new_grant_id();
    let kind = audience.kind();
    let client_id = audience.client_id();
    // Two PARTIAL uniques rather than one three-column key, so the conflict
    // target is spelled per audience arm. See the migration's header for why
    // the platform arm cannot share a key with the app arm.
    let sql = match audience {
        Audience::Platform => GRANT_UPSERT_PLATFORM_SQL,
        Audience::App { .. } => GRANT_UPSERT_APP_SQL,
    };
    let rows = db
        .query(
            sql,
            &[
                &id,
                &person_id.as_str(),
                &kind,
                &client_id,
                &subject,
                &scopes,
                &relay_email,
            ],
        )
        .await
        .map_err(|err| AuthError::Db(format!("grant upsert: {err}")))?;
    let row = rows
        .first()
        .ok_or_else(|| AuthError::Db("grant upsert: empty return".into()))?;
    Ok(row.get("id"))
}

const GRANT_UPSERT_PLATFORM_SQL: &str = "\
    INSERT INTO zeroship.grants \
        (id, person_id, audience_kind, client_id, subject, scopes, relay_email) \
    VALUES ($1, $2, $3, $4, $5, $6, $7) \
    ON CONFLICT (person_id) WHERE audience_kind = 'platform' DO UPDATE SET \
        scopes = EXCLUDED.scopes, \
        relay_email = COALESCE(EXCLUDED.relay_email, zeroship.grants.relay_email), \
        updated_at = NOW() \
    RETURNING id";

const GRANT_UPSERT_APP_SQL: &str = "\
    INSERT INTO zeroship.grants \
        (id, person_id, audience_kind, client_id, subject, scopes, relay_email) \
    VALUES ($1, $2, $3, $4, $5, $6, $7) \
    ON CONFLICT (person_id, client_id) WHERE audience_kind = 'app' DO UPDATE SET \
        scopes = EXCLUDED.scopes, \
        relay_email = COALESCE(EXCLUDED.relay_email, zeroship.grants.relay_email), \
        updated_at = NOW() \
    RETURNING id";

// ---------------------------------------------------------------------------
// Creation - the first validating read
// ---------------------------------------------------------------------------

/// What [`create`] needs. Everything else on the row is derived by the
/// statement from the person and the grant it reads.
#[derive(Debug)]
pub struct NewSession<'a> {
    pub person_id: &'a UserId,
    pub grant_id: &'a str,
    /// The grant's subject and scope ceiling, as the caller just wrote them.
    /// Carried in rather than re-read so [`create`] stays one statement.
    pub subject: &'a str,
    pub grant_scopes: &'a [String],
    pub parent_session_id: Option<&'a str>,
    pub kind: SessionKind,
    pub scopes: &'a [String],
    pub amr: &'a [String],
    pub acr: Option<&'a str>,
    pub label: Option<&'a str>,
    /// The credential epoch the authenticating event observed. `Some` pins the
    /// session to it, so a password change between authentication and issuance
    /// refuses rather than issuing against a stale authentication.
    pub expected_credential_epoch: Option<i64>,
    pub idle_days: i64,
    pub absolute_days: i64,
    /// Whether the session carries a presentable rotating secret. `false` for a
    /// token exchange granted no `offline_access`: that session is minted from
    /// once, by this statement, and can never be validated again.
    pub with_secret: bool,
}

/// A created session and the credential it hands back.
#[derive(Debug)]
pub struct CreatedSession {
    pub row: SessionRow,
    /// `None` when [`NewSession::with_secret`] was false.
    pub secret: Option<String>,
    pub proof: ValidatedSession,
}

/// Create a session, reading the person and the grant in the same statement.
///
/// Returns `Ok(None)` when the person is not active, the credential epoch has
/// moved, or the grant is suspended - all refusals rather than faults, and all
/// decided by the one `INSERT ... SELECT` below rather than by a preceding
/// check a later caller could forget.
///
/// # Errors
///
/// `AuthError::Db` when the statement fails.
pub async fn create(
    db: &(impl GenericClient + ?Sized),
    keys: &SessionSecretKeys,
    params: &NewSession<'_>,
) -> Result<Option<CreatedSession>> {
    let id = typed_id::new_session_id();
    let secret = params.with_secret.then(generate_secret);
    let hashed = secret.as_deref().map(|raw| keys.active_hash(raw));
    let secret_hash = hashed.as_ref().map(|h| h.hash.clone());
    let secret_version = hashed.as_ref().map(|h| h.version);
    let kind = params.kind.as_str();
    let idle_days = i32::try_from(params.idle_days).unwrap_or(7);
    let absolute_days = i32::try_from(params.absolute_days).unwrap_or(30);

    let rows = db
        .query(
            CREATE_SESSION_SQL,
            &[
                &id,
                &params.person_id.as_str(),
                &params.grant_id,
                &params.parent_session_id,
                &kind,
                &secret_hash,
                &secret_version,
                &params.amr,
                &params.acr,
                &params.scopes,
                &params.label,
                &idle_days,
                &absolute_days,
                &params.expected_credential_epoch,
            ],
        )
        .await
        .map_err(|err| AuthError::Db(format!("session create: {err}")))?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let row = row_to_session_with_grant(
        row,
        params.subject.to_string(),
        params.grant_scopes.to_vec(),
    )?;
    let proof = row.proof();
    Ok(Some(CreatedSession { row, secret, proof }))
}

/// The creating statement. It reads `zeroship.users` for liveness and the
/// credential epoch, and `zeroship.grants` for the audience and the suspension
/// status, and it writes the row it returns. That is what makes the first mint
/// of a session a mint from a validated read rather than an exception to the
/// rule.
const CREATE_SESSION_SQL: &str = "\
    INSERT INTO zeroship.sessions \
        (id, person_id, audience_kind, client_id, grant_id, parent_session_id, kind, \
         credential_epoch, secret_hash, secret_key_version, amr, acr, scopes, label, \
         idle_expires_at, absolute_expires_at) \
    SELECT $1, u.id, g.audience_kind, g.client_id, g.id, $4, $5, \
           u.credential_version, $6, $7, $8, $9, $10, $11, \
           LEAST(NOW() + make_interval(days => $12::INT), \
                 NOW() + make_interval(days => $13::INT)), \
           NOW() + make_interval(days => $13::INT) \
    FROM zeroship.users u \
    JOIN zeroship.grants g ON g.id = $3 AND g.person_id = u.id \
    WHERE u.id = $2 \
      AND ($14::BIGINT IS NULL OR u.credential_version = $14::BIGINT) \
      AND u.disabled_at IS NULL \
      AND u.anonymized_at IS NULL \
      AND u.deletion_requested_at IS NULL \
      AND u.deletion_scheduled_for IS NULL \
      AND (u.locked_until IS NULL OR u.locked_until <= NOW()) \
      AND g.subject_status = 'active' \
    RETURNING id, person_id, audience_kind, client_id, grant_id, kind, epoch, \
              credential_epoch, scopes, created_at, rotated_at, idle_expires_at, \
              absolute_expires_at, revoked_at, idem_response_enc, idem_expires_at, \
              secret_key_version, prev_secret_key_version, secret_hash, prev_secret_hash";

// ---------------------------------------------------------------------------
// Presentation - the validating read on every later mint
// ---------------------------------------------------------------------------

/// Resolve a presented secret to a session WITHOUT locking or validating it.
///
/// This exists so a caller can authenticate the OAuth client before it opens a
/// transaction, exactly as the refresh exchange did. It decides nothing: a
/// `Some` here is not permission to mint, and the only things that are, are
/// [`rotate`] and [`replay`].
///
/// # Errors
///
/// `AuthError::Db` when the lookup fails.
pub async fn peek(
    db: &(impl GenericClient + ?Sized),
    keys: &SessionSecretKeys,
    raw_secret: &str,
) -> Result<Option<PeekedSession>> {
    let hashes = keys.hashes_newest_first(raw_secret);
    if hashes.is_empty() {
        return Ok(None);
    }
    let candidates: Vec<Vec<u8>> = hashes.iter().map(|h| h.hash.clone()).collect();
    let rows = db
        .query(
            "SELECT id, person_id, client_id, secret_hash, prev_secret_hash \
             FROM zeroship.sessions \
             WHERE secret_hash = ANY($1::BYTEA[]) OR prev_secret_hash = ANY($1::BYTEA[])",
            &[&candidates],
        )
        .await
        .map_err(|err| AuthError::Db(format!("session peek: {err}")))?;
    for hash in hashes {
        for row in &rows {
            let current: Option<Vec<u8>> = row.try_get("secret_hash").ok().flatten();
            let superseded: Option<Vec<u8>> = row.try_get("prev_secret_hash").ok().flatten();
            let slot = if current.as_deref() == Some(hash.hash.as_slice()) {
                SecretSlot::Current
            } else if superseded.as_deref() == Some(hash.hash.as_slice()) {
                SecretSlot::Superseded
            } else {
                continue;
            };
            return Ok(Some(PeekedSession {
                session_id: row.get("id"),
                person_id: crate::user_id::from_row(row, "person_id", "session peek")?,
                client_id: row.try_get("client_id").ok().flatten(),
                slot,
                hash: hash.hash,
                version: hash.version,
            }));
        }
    }
    Ok(None)
}

/// Read a session and its grant `FOR UPDATE`, without validating either.
///
/// The caller needs the row's scope ceiling before it can decide what a
/// rotation may narrow to, so the read and the validating write are separate
/// statements. The write re-checks everything it depends on, so nothing rests
/// on what this read saw.
///
/// # Errors
///
/// `AuthError::Db` when the read fails.
pub async fn lock_and_read(
    db: &(impl GenericClient + ?Sized),
    session_id: &str,
) -> Result<Option<SessionRow>> {
    let rows = db
        .query(
            "SELECT s.id, s.person_id, s.audience_kind, s.client_id, s.grant_id, s.kind, \
                    s.epoch, s.credential_epoch, s.scopes, s.created_at, s.rotated_at, \
                    s.idle_expires_at, s.absolute_expires_at, s.revoked_at, \
                    s.idem_response_enc, s.idem_expires_at, s.secret_key_version, \
                    s.prev_secret_key_version, s.secret_hash, s.prev_secret_hash, \
                    g.subject, g.scopes AS grant_scopes \
             FROM zeroship.sessions s \
             JOIN zeroship.grants g ON g.id = s.grant_id \
             WHERE s.id = $1 \
             FOR UPDATE OF s",
            &[&session_id],
        )
        .await
        .map_err(|err| AuthError::Db(format!("session lock: {err}")))?;
    rows.first().map(row_to_session).transpose()
}

/// The validating read that rotates the secret and slides the idle window.
///
/// The validating update enforces session liveness, expiry, grant status,
/// person lifecycle and the credential epoch. The store integration tests in
/// `crates/zeroship-auth/tests/store/sessions.rs` exercise successful rotation
/// and refusal after eligibility changes against the migrated schema.
///
/// # Errors
///
/// `AuthError::Db` when the statement fails, or `AuthError::Internal` when the
/// idempotent response will not seal.
pub async fn rotate(
    db: &(impl GenericClient + ?Sized),
    keys: &SessionSecretKeys,
    presented: &PeekedSession,
    new_scopes: &[String],
    idle_days: i64,
    idem_window_secs: i64,
) -> Result<Option<RotatedSession>> {
    let new_secret = generate_secret();
    let new_hash = keys.active_hash(&new_secret);
    let cached = CachedResponse {
        refresh_token: new_secret.clone(),
        scope: new_scopes.join(" "),
    };
    let sealed = keys.seal_cached_response(&presented.hash, &presented.session_id, &cached)?;
    let idle_days = i32::try_from(idle_days).unwrap_or(7);
    let idem_secs = i32::try_from(idem_window_secs).unwrap_or(30);

    let rows = db
        .query(
            ROTATE_SESSION_SQL,
            &[
                &presented.session_id,
                &presented.hash,
                &new_hash.hash,
                &new_hash.version,
                &presented.version,
                &new_scopes,
                &idle_days,
                &sealed,
                &idem_secs,
            ],
        )
        .await
        .map_err(|err| AuthError::Db(format!("session rotate: {err}")))?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let row = row_to_session(row)?;
    let proof = row.proof();
    Ok(Some(RotatedSession {
        row,
        secret: new_secret,
        proof,
    }))
}

/// The rotating validating read.
///
/// `s.secret_hash = $2` is what makes this safe to run after a non-locking
/// peek: a concurrent rotation moves the hash, so the loser updates nothing and
/// is denied rather than both callers rotating.
const ROTATE_SESSION_SQL: &str = "\
    UPDATE zeroship.sessions s \
       SET secret_hash = $3, \
           secret_key_version = $4, \
           prev_secret_hash = s.secret_hash, \
           prev_secret_key_version = $5, \
           rotated_at = NOW(), \
           scopes = $6, \
           idle_expires_at = LEAST(NOW() + make_interval(days => $7::INT), \
                                   s.absolute_expires_at), \
           idem_response_enc = $8, \
           idem_expires_at = NOW() + make_interval(secs => $9::INT) \
      FROM zeroship.grants g, zeroship.users u \
     WHERE s.id = $1 \
       AND s.secret_hash = $2 \
       AND g.id = s.grant_id \
       AND u.id = s.person_id \
       AND s.revoked_at IS NULL \
       AND s.idle_expires_at > NOW() \
       AND s.absolute_expires_at > NOW() \
       AND s.credential_epoch = u.credential_version \
       AND g.subject_status = 'active' \
       AND u.disabled_at IS NULL \
       AND u.anonymized_at IS NULL \
       AND u.deletion_requested_at IS NULL \
       AND u.deletion_scheduled_for IS NULL \
    RETURNING s.id, s.person_id, s.audience_kind, s.client_id, s.grant_id, s.kind, \
              s.epoch, s.credential_epoch, s.scopes, s.created_at, s.rotated_at, \
              s.idle_expires_at, s.absolute_expires_at, s.revoked_at, \
              s.idem_response_enc, s.idem_expires_at, s.secret_key_version, \
              s.prev_secret_key_version, s.secret_hash, s.prev_secret_hash, \
              g.subject, g.scopes AS grant_scopes";

/// The single retry a lost rotation response earns.
///
/// `locked` must be the row a [`lock_and_read`] in this transaction returned,
/// because the sealed record cannot be read back out of the consuming statement
/// on a server without `RETURNING OLD`, and the lock is what makes the value
/// this reads still the value the statement consumes.
///
/// Returns `Ok(None)` for every presentation that is NOT that retry, and the
/// caller must treat every one of them as reuse. Nothing here may answer with a
/// refusal of its own: an early return would skip the caller's kill, so a
/// record that will not open - which is what a rotated idempotency key looks
/// like - would DISARM reuse detection instead of triggering it.
///
/// # Errors
///
/// `AuthError::Db` when the consuming statement fails.
pub async fn replay(
    db: &(impl GenericClient + ?Sized),
    keys: &SessionSecretKeys,
    presented: &PeekedSession,
    locked: &SessionRow,
) -> Result<Option<(CachedResponse, SessionRow, ValidatedSession)>> {
    let Some(sealed) = locked.idem_response_enc.clone() else {
        return Ok(None);
    };
    // Single-use by construction: a conditional UPDATE rather than a
    // read-then-write, so two concurrent replays cannot both observe the record
    // present and both be served. The liveness predicates ride the same
    // statement, so this is a validating read like every other mint site.
    let rows = db
        .query(CONSUME_IDEM_SQL, &[&presented.session_id, &presented.hash])
        .await
        .map_err(|err| AuthError::Db(format!("session replay consume: {err}")))?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let row = row_to_session(row)?;
    let Ok(cached) = keys.open_cached_response(&presented.hash, &row.id, &sealed) else {
        tracing::warn!(
            session_id = %row.id,
            "session idempotency record would not open; failing closed and treating the \
             presentation as reuse"
        );
        return Ok(None);
    };
    let proof = row.proof();
    Ok(Some((cached, row, proof)))
}

const CONSUME_IDEM_SQL: &str = "\
    UPDATE zeroship.sessions s \
       SET idem_response_enc = NULL, \
           idem_expires_at = NULL \
      FROM zeroship.grants g, zeroship.users u \
     WHERE s.id = $1 \
       AND s.prev_secret_hash = $2 \
       AND s.idem_response_enc IS NOT NULL \
       AND s.idem_expires_at > NOW() \
       AND g.id = s.grant_id \
       AND u.id = s.person_id \
       AND s.revoked_at IS NULL \
       AND s.idle_expires_at > NOW() \
       AND s.absolute_expires_at > NOW() \
       AND s.credential_epoch = u.credential_version \
       AND g.subject_status = 'active' \
       AND u.disabled_at IS NULL \
       AND u.anonymized_at IS NULL \
       AND u.deletion_requested_at IS NULL \
       AND u.deletion_scheduled_for IS NULL \
    RETURNING s.id, s.person_id, s.audience_kind, s.client_id, s.grant_id, s.kind, \
              s.epoch, s.credential_epoch, s.scopes, s.created_at, s.rotated_at, \
              s.idle_expires_at, s.absolute_expires_at, s.revoked_at, \
              s.idem_response_enc, s.idem_expires_at, s.secret_key_version, \
              s.prev_secret_key_version, s.secret_hash, s.prev_secret_hash, \
              g.subject, g.scopes AS grant_scopes";

// ---------------------------------------------------------------------------
// Revocation
// ---------------------------------------------------------------------------

/// End one session, and recall the access tokens it minted.
///
/// The marker in `zeroship.token_revocations` is what recalls a token already
/// in a client's hands; the row update is what stops the next mint. Both in one
/// statement, so either both commit or neither does.
///
/// A platform-audience grant carries no `client_id` - the column is NULL by the
/// `grants_audience_shape` CHECK - and the marker is keyed on the client the
/// access token names. The platform audience is the first-party CLI and nothing
/// else (`platform_cli_policy_selected` returns false for every other client
/// id), so that is the key the fallback supplies. A marker under any other name
/// would revoke a subject nothing presents.
///
/// # Errors
///
/// `AuthError::Db` when the statement fails.
pub async fn revoke(
    db: &(impl GenericClient + ?Sized),
    session_id: &str,
    reason: &'static str,
) -> Result<()> {
    db.execute(
        "WITH target AS ( \
             SELECT DISTINCT g.client_id, g.subject \
             FROM zeroship.sessions s \
             JOIN zeroship.grants g ON g.id = s.grant_id \
             WHERE s.id = $1 \
         ), upd AS ( \
             UPDATE zeroship.sessions \
             SET revoked_at = NOW() \
             WHERE id = $1 AND revoked_at IS NULL \
             RETURNING 1 \
         ) \
         INSERT INTO zeroship.token_revocations (client_id, sub, revoked_after) \
         SELECT COALESCE(client_id, $2), subject, NOW() FROM target \
         ON CONFLICT (client_id, sub) \
           DO UPDATE SET revoked_after = \
             GREATEST(zeroship.token_revocations.revoked_after, EXCLUDED.revoked_after)",
        &[&session_id, &PLATFORM_CLI_CLIENT_ID],
    )
    .await
    .map_err(|err| AuthError::Db(format!("session revoke ({reason}): {err}")))?;
    tracing::info!(session_id, reason, "session revoked");
    Ok(())
}

/// End every live session a person holds, inside the caller's transaction.
///
/// # Errors
///
/// A message naming the reason when the statement fails.
pub async fn revoke_person_sessions(
    db: &(impl GenericClient + ?Sized),
    person_id: &UserId,
    reason: &'static str,
) -> std::result::Result<u64, String> {
    let rows = db
        .query(
            // DISTINCT is load-bearing, not tidiness: a person with two live
            // sessions on ONE grant yields the same (client_id, subject) twice,
            // and `ON CONFLICT DO UPDATE` refuses to affect one row twice in a
            // single command. Without it this statement fails exactly when it
            // matters most - the person with several sessions to end.
            "WITH target AS ( \
                 SELECT DISTINCT g.client_id, g.subject \
                 FROM zeroship.sessions s \
                 JOIN zeroship.grants g ON g.id = s.grant_id \
                 WHERE s.person_id = $1 AND s.revoked_at IS NULL \
             ), upd AS ( \
                 UPDATE zeroship.sessions \
                 SET revoked_at = clock_timestamp() \
                 WHERE person_id = $1 AND revoked_at IS NULL \
                 RETURNING 1 \
             ), marked AS ( \
                 INSERT INTO zeroship.token_revocations (client_id, sub, revoked_after) \
                 SELECT COALESCE(client_id, $2), subject, clock_timestamp() FROM target \
                 ON CONFLICT (client_id, sub) \
                   DO UPDATE SET revoked_after = \
                     GREATEST(zeroship.token_revocations.revoked_after, EXCLUDED.revoked_after) \
                 RETURNING 1 \
             ) \
             SELECT count(*) AS revoked FROM upd",
            &[&person_id.as_str(), &PLATFORM_CLI_CLIENT_ID],
        )
        .await
        .map_err(|err| format!("session revoke for person ({reason}): {err}"))?;
    let revoked: i64 = rows.first().map_or(0, |row| row.get("revoked"));
    Ok(u64::try_from(revoked).unwrap_or(0))
}

/// Delete sessions whose absolute ceiling passed more than `retention_days`
/// ago, and clear idempotent records past their own window.
///
/// # Errors
///
/// A message when either statement fails.
pub async fn sweep(
    db: &(impl GenericClient + ?Sized),
    retention_days: i64,
) -> std::result::Result<(u64, u64), String> {
    let retention = i32::try_from(retention_days).unwrap_or(30);
    let deleted = db
        .execute(
            "DELETE FROM zeroship.sessions \
             WHERE absolute_expires_at < NOW() - make_interval(days => $1::INT)",
            &[&retention],
        )
        .await
        .map_err(|err| format!("session sweep delete: {err}"))?;
    // The sealed response expires on its own deadline. `prev_secret_hash`
    // deliberately survives it: the two are different windows, and clearing the
    // hash here would shrink reuse detection to the replay window.
    let idem = db
        .execute(
            "UPDATE zeroship.sessions \
             SET idem_response_enc = NULL, idem_expires_at = NULL \
             WHERE idem_expires_at IS NOT NULL AND idem_expires_at <= NOW()",
            &[],
        )
        .await
        .map_err(|err| format!("session sweep idempotency: {err}"))?;
    Ok((deleted, idem))
}

// ---------------------------------------------------------------------------

fn row_to_session(row: &Row) -> Result<SessionRow> {
    row_to_session_with_grant(row, row.get("subject"), row.get("grant_scopes"))
}

fn row_to_session_with_grant(
    row: &Row,
    subject: String,
    grant_scopes: Vec<String>,
) -> Result<SessionRow> {
    Ok(SessionRow {
        id: row.get("id"),
        person_id: crate::user_id::from_row(row, "person_id", "session row")?,
        audience_kind: row.get("audience_kind"),
        client_id: row.try_get("client_id").ok().flatten(),
        grant_id: row.get("grant_id"),
        kind: row.get("kind"),
        epoch: row.get("epoch"),
        credential_epoch: row.get("credential_epoch"),
        scopes: row.get("scopes"),
        grant_scopes,
        subject,
        created_at: row.get("created_at"),
        rotated_at: row.try_get("rotated_at").ok().flatten(),
        idle_expires_at: row.get("idle_expires_at"),
        absolute_expires_at: row.get("absolute_expires_at"),
        revoked_at: row.try_get("revoked_at").ok().flatten(),
        idem_response_enc: row.try_get("idem_response_enc").ok().flatten(),
        idem_expires_at: row.try_get("idem_expires_at").ok().flatten(),
        secret_key_version: row.try_get("secret_key_version").ok().flatten(),
        prev_secret_key_version: row.try_get("prev_secret_key_version").ok().flatten(),
        secret_hash: row.try_get("secret_hash").ok().flatten(),
        prev_secret_hash: row.try_get("prev_secret_hash").ok().flatten(),
    })
}
