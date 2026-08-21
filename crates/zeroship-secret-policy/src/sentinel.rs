//! The named sentinel every unset service credential collapses to.
//!
//! Split out of `zeroship_core::config::credential_gate` when this crate was
//! made a leaf: the validators in [`crate`] need the predicate and the message,
//! and pulling them from core would have put the whole HTTP stack behind a gate
//! that reads a YAML file. The AUDIT machinery those two feed -
//! `SubsystemCredential`, `CredentialPosture`, `audit_credentials`, the boot
//! banner and the build-profile dev escape - stayed in core, because it is
//! typed on core's `Secret` and `ConfigSource`.
//!
//! NOTHING HERE IS TESTED IN THIS CRATE, deliberately. The properties that
//! matter about these four items are relational - "empty and the sentinel
//! produce byte-identical refusals", driven over every row of
//! [`crate::PLATFORM_SECRETS`] - and every one of those tests needs the audit
//! types that stayed behind. They live in
//! `zeroship_core::config::credential_gate`'s test module and drive this code
//! across the crate boundary. A reader who greps this file for `#[test]` and
//! finds none has not found an untested module.

/// The placeholder an operator is meant to replace, and which the platform
/// refuses to start on.
///
/// DELIBERATELY NOT RANDOM-LOOKING. A default like `changeme123` or a 32-byte
/// hex string reads as a configured value in a diff, in a `docker compose
/// config` dump and in a boot log; this one cannot. It is also shorter than
/// [`crate::MIN_SECRET_BYTES`] (30 bytes against 32), which matters:
/// a reader must not be able to conclude "the length floor catches it anyway",
/// because the two secrets this gate exists for -- `ZEROSHIP_CONTROL_KEY` and
/// `ZEROSHIP_MIGRATED_POLICY_SEAL_KEY` -- are
/// [`crate::SecretStrength::Unrestricted`] and have no length floor at
/// all. The sentinel branch, not the length branch, is what refuses them.
pub const SERVICE_CREDENTIAL_SENTINEL: &str = "CHANGE_ME_ZEROSHIP_SERVICE_KEY";

/// The exact command that provisions every platform secret.
///
/// Interpolated into the banner so the remediation is a command an operator can
/// paste, not a description of one.
pub const REMEDIATION_COMMAND: &str = "zeroship dev init";

/// True when `value` carries no credential at all.
///
/// THE SINGLE BRANCH. Empty and [`SERVICE_CREDENTIAL_SENTINEL`] are the same
/// state -- "nobody set this" -- and they are one predicate so that no later
/// edit can give one of them a different fate without giving it to the other.
/// That is the whole of Gitaly's bug: it had a branch for the empty token and
/// the branch returned success.
///
/// Surrounding whitespace is trimmed before the comparison, because a `.env`
/// file and a compose `environment:` block both pick up a trailing space
/// without the operator seeing it, and `CHANGE_ME_ZEROSHIP_SERVICE_KEY ` is the
/// same unreplaced placeholder as `CHANGE_ME_ZEROSHIP_SERVICE_KEY`. The trim
/// applies to BOTH arms, so the two stay symmetric.
#[must_use]
pub fn is_unset_credential(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.is_empty() || trimmed == SERVICE_CREDENTIAL_SENTINEL
}

/// The refusal every validator produces for an unset credential.
///
/// One format string shared by every caller, so the message an operator reads
/// for an empty value and the message they read for the sentinel are the same
/// bytes. `label` is the caller's operator-facing spelling of the variable.
#[must_use]
pub fn unset_credential_message(label: &str) -> String {
    format!(
        "{label} is required and is not configured: it is empty or still the \
         {SERVICE_CREDENTIAL_SENTINEL} placeholder; run `{REMEDIATION_COMMAND}`"
    )
}
