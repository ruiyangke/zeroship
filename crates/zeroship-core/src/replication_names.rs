//! Stable names shared by migration-time and worker-time CDC code.

use sha2::{Digest, Sha256};

/// Prefix reserved for zeroship logical-replication objects.
pub const OBJECT_PREFIX: &str = "__zs_";

/// The ONE publication on a datastore, owned by the relay.
///
/// It is a constant and not a derivation, and that is the design rather than a
/// simplification. A publication is an object of one `PostgreSQL` database, and
/// a datastore is reached through one; so "one publication per datastore" has
/// exactly one name to compose and nothing to compose it from. Naming it after
/// an app or a database would say the opposite of what this object is.
///
/// **Its membership is not a tenant filter.** The publication deliberately
/// spans every database on the datastore, so a consumer that read namespaces
/// out of it would admit every co-tenant's relations. What separates tenants in
/// a stream is the decoder's comparison against the schema of the ONE database
/// a subscriber is bound to.
///
/// **A database edits only its own member entries, under the datastore
/// publication mutex.** `ALTER PUBLICATION ... SET TABLE` names the whole
/// object, so one database issuing it would drop every other database's tables
/// out of the shared stream without an error anywhere.
pub const DATASTORE_PUBLICATION: &str = "__zs_pub_datastore";

/// Prefix reserved for the relay's own replication slots.
///
/// The relay reclaims slots by scanning for this prefix and nothing else, so a
/// name that lost it is a slot nothing ever drops, and nothing but a relay slot
/// may carry it. The scan is over the prefix alone and not over an app, so an
/// app owning several slots costs it nothing.
///
/// A slot is per (app, database), the granularity the relay captures at: one
/// capture task runs per subscribed pair and a logical slot admits exactly one
/// consumer, so a name that carried less than the pair would have two captures
/// request one slot. Slots still replicate decode work rather than partitioning
/// it - each one decodes the whole datastore publication and compares
/// namespaces - so the pair is in the name to give every capture a slot of its
/// own, never to narrow what any of them reads.
pub const SLOT_PREFIX: &str = "__zs_relay_";

/// `PostgreSQL`'s bound on a replication slot name, in bytes.
///
/// Numerically `NAMEDATALEN - 1`, the width an identifier also gets, but a
/// different limit reached by a different route - and what the server does
/// about a longer name depends on how the name arrives. BOUND as the `name`
/// argument of `pg_create_logical_replication_slot`, which is the relay's own
/// path, it is refused: `42622`, `identifier too long`. Reaching that argument
/// through a server-side text-to-`name` conversion instead, it is clipped to
/// this width with neither an error nor a notice. So an over-long name is
/// either a capture that cannot start or a name nothing composed, and never a
/// working slot.
///
/// `PostgreSQL` narrows the CHARACTERS too, further than a quoted identifier:
/// `ReplicationSlotValidateName` admits lower-case letters, digits and the
/// underscore and refuses everything else with `42602`. That is why every
/// composer here emits a fixed prefix and lower-case hexadecimal rather than a
/// caller's own spelling.
///
/// Both halves are bound to a running server by
/// `postgresql_bounds_a_slot_name_and_the_characters_it_may_carry`
/// (`crates/zeroship-data-cdc-server/src/source.rs`), so this comment is not
/// where the claim rests.
pub const POSTGRES_SLOT_NAME_MAX_BYTES: usize = 63;

/// Bytes of SHA-256 kept in the app half of a relay slot name.
const APP_TOKEN_BYTES: usize = 14;

/// Bytes of SHA-256 kept in the database half of a relay slot name.
const DATABASE_TOKEN_BYTES: usize = 10;

/// What separates the two halves of a relay slot name.
///
/// Hexadecimal carries no underscore, so a pair of them occurs in neither
/// token and the halves stay legible back out of a slot name.
const TOKEN_SEPARATOR: &str = "__";

/// A relay slot name FITS, and this is where that is decided.
///
/// Both halves are fixed-width tokens, so the composed width is a property of
/// this file and of no input - which is what makes it checkable here, before
/// anything runs. Widening a token or the prefix past the bound fails the build
/// rather than the fleet, and no shortened name is composed on the way.
///
/// What crossing the bound costs, on the two paths
/// [`POSTGRES_SLOT_NAME_MAX_BYTES`] describes. On the relay's own, the server
/// refuses the name outright and every capture stops at slot creation. On one
/// that converts text to `name`, the tail goes silently - and the database half
/// is LAST, so the bytes that go are exactly the ones telling one app's two
/// captures apart, leaving them one slot, which admits one consumer.
const _: () = assert!(
    SLOT_PREFIX.len() + APP_TOKEN_BYTES * 2 + TOKEN_SEPARATOR.len() + DATABASE_TOKEN_BYTES * 2
        <= POSTGRES_SLOT_NAME_MAX_BYTES,
    "a relay slot name must fit PostgreSQL's bound: past it the server refuses \
     the name on the relay's own path, and drops the tail without a word on one \
     that converts text to `name`"
);

/// Why a caller-controlled identifier cannot name a CDC object.
///
/// The app half and the database half get their own variants rather than one
/// shared "bad id". A relay slot is named for a pair, and a refusal that did
/// not say which half was unusable would point an operator at the wrong side of
/// a subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReplicationNameError {
    #[error("app id must not be empty")]
    EmptyAppId,
    #[error("app id must not contain NUL")]
    NulAppId,
    #[error("database id must not be empty")]
    EmptyDatabaseId,
    #[error("database id must not contain NUL")]
    NulDatabaseId,
}

/// Compose the relay slot name for ONE capture: one app on one database.
///
/// The relay runs a capture per (app, database) pair and a logical slot admits
/// exactly one consumer, so both halves are in the name. An app holding live
/// bindings to two databases of one datastore runs two captures against one
/// `PostgreSQL` database; a name composed from the app alone has them request
/// one slot, and the server answers the second with `42710`, after which that
/// capture ends and its subscribers disconnect.
///
/// Neither id is spelled into the name. Both are hashed, which is what holds
/// the composed name inside [`POSTGRES_SLOT_NAME_MAX_BYTES`] and inside the
/// lower-case-alphanumeric-and-underscore set a slot name may use, whatever
/// text a caller hands in.
///
/// # Errors
///
/// [`ReplicationNameError`] when either id is empty or carries a NUL. Neither
/// is an identity the control plane mints, and the pair is what makes two
/// captures two slots, so a half that is not an identity cannot do that job.
pub fn relay_slot_name(app_id: &str, database_id: &str) -> Result<String, ReplicationNameError> {
    if app_id.is_empty() {
        return Err(ReplicationNameError::EmptyAppId);
    }
    if app_id.contains('\0') {
        return Err(ReplicationNameError::NulAppId);
    }
    if database_id.is_empty() {
        return Err(ReplicationNameError::EmptyDatabaseId);
    }
    if database_id.contains('\0') {
        return Err(ReplicationNameError::NulDatabaseId);
    }
    Ok(format!(
        "{SLOT_PREFIX}{}{TOKEN_SEPARATOR}{}",
        stable_token(app_id, APP_TOKEN_BYTES),
        stable_token(database_id, DATABASE_TOKEN_BYTES)
    ))
}

/// The first `bytes` of SHA-256 over `value`, as lower-case hexadecimal.
fn stable_token(value: &str, bytes: usize) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut token = String::with_capacity(bytes * 2);
    for byte in &digest[..bytes] {
        use std::fmt::Write as _;
        write!(&mut token, "{byte:02x}").expect("writing to String cannot fail");
    }
    token
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "alpha";
    const DATABASE: &str = "dbs_one";

    #[test]
    fn slot_names_are_stable_case_sensitive_identifiers() {
        assert_eq!(
            relay_slot_name(APP, DATABASE).unwrap(),
            "__zs_relay_8ed3f6ad685b959ead7022518e1a__f7835d6ef924dbdd4c90"
        );
        assert_ne!(
            relay_slot_name("MyApp", DATABASE).unwrap(),
            relay_slot_name("myapp", DATABASE).unwrap()
        );
        assert_ne!(
            relay_slot_name(APP, "DBS_One").unwrap(),
            relay_slot_name(APP, "dbs_one").unwrap()
        );
        assert_eq!(
            relay_slot_name("", DATABASE),
            Err(ReplicationNameError::EmptyAppId)
        );
        assert_eq!(
            relay_slot_name("bad\0app", DATABASE),
            Err(ReplicationNameError::NulAppId)
        );
        assert_eq!(
            relay_slot_name(APP, ""),
            Err(ReplicationNameError::EmptyDatabaseId)
        );
        assert_eq!(
            relay_slot_name(APP, "bad\0db"),
            Err(ReplicationNameError::NulDatabaseId)
        );
    }

    /// **The defect this name shape exists to prevent.**
    ///
    /// One app subscribing to two of its databases runs two captures against
    /// ONE `PostgreSQL` database, so the two names have to differ or the second
    /// capture asks for a slot the first already holds. The second half of the
    /// arm is the control from the other direction: two apps reaching one
    /// shared database are also two captures, so the app half has to vary too.
    /// The pair-wise assertion at the end is what refuses a composer that
    /// answered from either half alone.
    #[test]
    fn one_app_s_two_databases_compose_two_slot_names() {
        let mine = relay_slot_name(APP, "dbs_one").unwrap();
        let theirs = relay_slot_name(APP, "dbs_two").unwrap();
        assert_ne!(
            mine, theirs,
            "one app's two databases must not request one slot"
        );

        let neighbour = relay_slot_name("beta", "dbs_one").unwrap();
        assert_ne!(
            mine, neighbour,
            "two apps bound to one database must not request one slot"
        );

        let names = [
            mine,
            theirs,
            neighbour,
            relay_slot_name("beta", "dbs_two").unwrap(),
        ];
        let mut distinct = names.to_vec();
        distinct.sort();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            names.len(),
            "every (app, database) pair is its own slot; composed {names:?}"
        );
    }

    /// The relay drops slots by [`SLOT_PREFIX`] and nothing else, so a name
    /// that lost the prefix would be a slot the relay never reclaims.
    #[test]
    fn every_slot_name_carries_the_prefix_the_relay_reclaims_by() {
        assert!(relay_slot_name(APP, DATABASE)
            .unwrap()
            .starts_with(SLOT_PREFIX));
        assert!(SLOT_PREFIX.starts_with(OBJECT_PREFIX));
    }

    /// The shared publication and the per-capture slots must not collide in the
    /// reserved namespace: the relay's prefix-scoped slot drop would otherwise
    /// name the publication.
    #[test]
    fn the_datastore_publication_is_not_reachable_by_the_slot_prefix() {
        assert!(DATASTORE_PUBLICATION.starts_with(OBJECT_PREFIX));
        assert!(!DATASTORE_PUBLICATION.starts_with(SLOT_PREFIX));
    }

    /// `PostgreSQL` refuses anything outside `[a-z0-9_]` in a slot name, and
    /// the ids reaching this composer are text a caller chose.
    ///
    /// The control is the last loop: each input is itself shown to carry a
    /// character the server would refuse, so the arm is not passing over
    /// inputs that were already legal.
    #[test]
    fn a_composed_slot_name_uses_only_the_characters_postgresql_admits() {
        let admissible = |name: &str| {
            name.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        };
        let refused = ["App_ONE", "app-one", "app.one", "app one", "\u{e9}pp"];
        let mut checked = 0;
        for id in refused {
            assert!(
                admissible(&relay_slot_name(id, DATABASE).unwrap()),
                "an app id spelled `{id}` must still compose an admissible slot name"
            );
            assert!(
                admissible(&relay_slot_name(APP, id).unwrap()),
                "a database id spelled `{id}` must still compose an admissible slot name"
            );
            assert!(
                !admissible(id),
                "the control: `{id}` must itself be a spelling PostgreSQL refuses"
            );
            checked += 1;
        }
        assert_eq!(
            checked,
            refused.len(),
            "the arm must not pass over an empty list"
        );
    }

    /// The composed width is a constant of this file, so neither thing
    /// [`POSTGRES_SLOT_NAME_MAX_BYTES`] describes can happen to a relay slot.
    ///
    /// Hashing is what buys that: the ids are caller text of any length, and
    /// the name they compose is the same length every time. The second half of
    /// the arm exhibits the quieter of the two costs - the database half is
    /// last, so bytes clipped off the end are exactly the ones that tell one
    /// app's two captures apart.
    #[test]
    fn every_composed_slot_name_fits_the_bound_whatever_the_ids_spell() {
        let width = relay_slot_name(APP, DATABASE).unwrap().len();
        assert!(width <= POSTGRES_SLOT_NAME_MAX_BYTES);
        let mut checked = 0;
        for (app, database) in [
            ("a", "d"),
            (APP, DATABASE),
            (
                "app_03cgepu94hyemwpcipafo7264",
                "dbs_03cgepu94hyemwpcipafo7264",
            ),
            (&"a".repeat(4096), &"d".repeat(4096)),
            ("\u{1f600}\u{e9}", "\u{4e2d}\u{6587}"),
        ] {
            assert_eq!(
                relay_slot_name(app, database).unwrap().len(),
                width,
                "the composed width must not depend on how long an id is"
            );
            checked += 1;
        }
        assert_eq!(checked, 5, "the arm must not pass over an empty list");

        // The collision the bound prevents, built out of the same pieces the
        // composer uses rather than out of the composer, so the shape is
        // checked too: two names that differ only in their database half are
        // ONE name once the tail is gone.
        let over = POSTGRES_SLOT_NAME_MAX_BYTES - SLOT_PREFIX.len() - TOKEN_SEPARATOR.len();
        let head = format!("{SLOT_PREFIX}{}{TOKEN_SEPARATOR}", "a".repeat(over));
        let first = format!("{head}1");
        let second = format!("{head}2");
        assert_ne!(first, second, "two databases are two names in full");
        assert_eq!(
            first[..POSTGRES_SLOT_NAME_MAX_BYTES],
            second[..POSTGRES_SLOT_NAME_MAX_BYTES],
            "clipped to the bound, one app's two captures are one slot"
        );
        assert!(
            first.len() > POSTGRES_SLOT_NAME_MAX_BYTES && width < first.len(),
            "the control: the composer's own output is shorter than the name \
             that would be clipped"
        );
    }
}
