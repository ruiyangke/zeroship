//! The part of the secret policy that needs core's own config machinery.
//!
//! THE POLICY ITSELF IS NOT HERE. [`zeroship_secret_policy`] owns the secret
//! table, the strength rules, every validator and the reference parser; this
//! module is what could not follow it, because it is typed on things that live
//! in `zeroship-core`:
//!
//! * [`validate_secret_material`] takes a [`crate::config::Secret<String>`],
//!   which is `config::names` machinery;
//! * the message-hygiene test below drives the leaf crate's validators through
//!   [`crate::config::env_like_tokens`], the shared scanner the control and
//!   migrated binaries also read.
//!
//! [`crate::config`] does NOT re-export the leaf crate's items beside this one.
//! A caller that wants the table or a validator names `zeroship_secret_policy`
//! and takes the dependency; a caller that wants this bridge names
//! `zeroship_core::config`. Two crates to import from is the visible cost, and
//! it is the price of one public path per symbol.

/// Run a strength validator against a resolved secret's material.
///
/// The single bridge between [`crate::config::Secret`] and the `&str`
/// validators in [`zeroship_secret_policy`], so every binary answers "does this
/// secret still get validated" the same way. Three cases, and only the middle
/// one is new:
///
/// * material present (any real boot, and a `--check-config` run whose secret
///   is an in-memory literal) - the validator runs on the real material;
/// * configured but unread - only reachable under `--check-config` for a source
///   that would need I/O. There is nothing to check, and checking the reference
///   TEXT instead of the secret is what the old `is_secret_ref` dance did;
/// * unsupplied - the validator runs on `""`, which is how each one already
///   produces its own "X is required" message rather than a generic one.
///
/// # Errors
///
/// Propagates the validator's message unchanged.
pub fn validate_secret_material<F>(
    secret: &crate::config::Secret<String>,
    validate: F,
) -> Result<(), String>
where
    F: FnOnce(&str) -> Result<(), String>,
{
    match secret.expose_secret() {
        Some(material) => validate(material),
        None if secret.is_configured() => Ok(()),
        None => validate(""),
    }
}

#[cfg(test)]
mod tests {
    use zeroship_secret_policy::{
        require_nonempty, validate_master_key_material, validate_pairwise_salt, validate_stash_key,
        validate_worker_key, KNOWN_WEAK_MASTER_KEYS, KNOWN_WEAK_PAIRWISE_SALTS,
        KNOWN_WEAK_STASH_KEYS, KNOWN_WEAK_WORKER_KEYS,
    };

    use super::validate_secret_material;
    use crate::config::{Secret, SourceKind};

    const SENTINEL: &str = "ZEROSHIP_SENTINEL_LABEL";

    /// THE DEFECT. Three validators interpolated a bare `STASH_SIGNING_KEY`,
    /// `PAIRWISE_SALT` and `WORKER_KEY` into their refusals. None of those is a
    /// variable any binary reads: the shared identities in
    /// `crates/config-macros/src/shared.rs` project to `ZEROSHIP_WORKER_KEY`
    /// and `ZEROSHIP_PAIRWISE_SALT`, and the stash key is not shared at all -
    /// it is `gateway.stash_signing_key` and `auth.stash_signing_key`, two
    /// different variables behind one validator. An operator who followed any
    /// of these refusals set a variable the binary does not read.
    ///
    /// The fix is that this module names NOTHING. Every refusal carries only
    /// the caller's label, so the operator-facing spelling lives next to the
    /// declaration it must agree with, where each binary's own diagnostic test
    /// checks it against the set of names that binary really reads.
    #[test]
    fn every_refusal_names_the_callers_label_and_invents_no_name_of_its_own() {
        // Drive every message-producing arm of every labelled validator.
        let messages: Vec<String> = [
            validate_stash_key(SENTINEL, KNOWN_WEAK_STASH_KEYS[0]),
            validate_stash_key(SENTINEL, ""),
            validate_stash_key(SENTINEL, "short"),
            validate_pairwise_salt(SENTINEL, KNOWN_WEAK_PAIRWISE_SALTS[0]),
            validate_pairwise_salt(SENTINEL, ""),
            validate_pairwise_salt(SENTINEL, "short"),
            validate_worker_key(SENTINEL, KNOWN_WEAK_WORKER_KEYS[0]),
            validate_worker_key(SENTINEL, ""),
            validate_worker_key(SENTINEL, "short"),
            validate_master_key_material(SENTINEL, KNOWN_WEAK_MASTER_KEYS[0]),
            validate_master_key_material(SENTINEL, "YWJj"),
            validate_master_key_material(SENTINEL, "not!base64!"),
            require_nonempty(SENTINEL, ""),
        ]
        .into_iter()
        .map(|result| result.expect_err("each input above must be refused"))
        .collect();

        for message in &messages {
            let tokens = crate::config::env_like_tokens(message);
            assert_eq!(
                tokens,
                vec![SENTINEL.to_owned()],
                "refusal {message:?} must name the caller's label and nothing else; \
                 a name spelled inside this module is invisible to the config \
                 contract and rots when the declaration is renamed"
            );
        }

        // The one-variable partner. Same scanner, same messages, only the label
        // changes - and a DIFFERENT label must show through, so the assertion
        // above cannot be passing because the scanner sees nothing or because
        // the messages are constant.
        let other = "ZEROSHIP_OTHER_LABEL";
        assert_eq!(
            crate::config::env_like_tokens(
                &validate_worker_key(other, "short").expect_err("short key")
            ),
            vec![other.to_owned()]
        );

        // Does NOT cover whether the label a given binary passes is a name that
        // binary actually reads. That is per-binary wiring, asserted by each
        // binary's own `every_startup_diagnostic_names_a_variable_*_reads` test.
    }

    // The bridge every binary uses to keep its strength validators running on
    // the RESOLVED material. Each arm is paired with a control differing in one
    // variable, because the interesting failure is the middle one silently
    // swallowing a real boot.
    #[test]
    fn a_strength_validator_runs_on_material_and_skips_only_the_unread_case() {
        let seen = std::cell::RefCell::new(Vec::new());
        let record = |value: &str| -> Result<(), String> {
            seen.borrow_mut().push(value.to_owned());
            if value.len() >= 32 { Ok(()) } else { Err(format!("too short: {}", value.len())) }
        };

        // 1. Material present: the validator sees the real material.
        let strong = "0123456789abcdef0123456789abcdef";
        validate_secret_material(
            &Secret::supplied(SourceKind::Env, Some(strong.to_owned())),
            record,
        )
        .expect("strong material passes");
        assert_eq!(seen.borrow().as_slice(), [strong.to_owned()]);

        // 1b. The one-variable partner: weak material still FAILS, so arm 1 is
        // not passing because the validator was never called.
        seen.borrow_mut().clear();
        validate_secret_material(
            &Secret::supplied(SourceKind::Env, Some("short".to_owned())),
            record,
        )
        .expect_err("weak material is rejected");
        assert_eq!(seen.borrow().as_slice(), ["short".to_owned()]);

        // 2. Configured but unread - only a dry run reaches this. Nothing to
        // check, so the validator must not be called at all.
        seen.borrow_mut().clear();
        validate_secret_material(&Secret::supplied(SourceKind::CliFile, None), record)
            .expect("an unread secret is not judged");
        assert!(seen.borrow().is_empty(), "the validator must not run on nothing");

        // 3. Unsupplied: the validator runs on "" and produces its own message.
        seen.borrow_mut().clear();
        let error = validate_secret_material(&Secret::<String>::absent(), record)
            .expect_err("an unset secret is rejected");
        assert_eq!(error, "too short: 0");
        assert_eq!(seen.borrow().as_slice(), [String::new()]);

        // Does NOT cover whether any given binary actually calls this. That is
        // per-binary wiring, asserted by each binary's own boot-guard tests.
    }
}
