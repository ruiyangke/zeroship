//! The boot gate against a silent weak service credential.
//!
//! Modelled on Superset's post-CVE fix for CVE-2023-27524 (CVSS 9.8), where a
//! shipped default `SECRET_KEY` that nobody changed forged session cookies on
//! thousands of internet-facing installs. The fix was not "warn louder": it was
//! a NAMED sentinel that the product refuses to start on.
//!
//! Five properties, each of which exists because some project shipped its
//! opposite:
//!
//! 1. **A named sentinel**, [`SERVICE_CREDENTIAL_SENTINEL`], never a
//!    plausible-looking random string. The whole value of the constant is that
//!    it is unmistakable in a config file, in a boot log and in a grep.
//! 2. **Empty and sentinel are ONE state.** Gitaly's
//!    `if len(conf.GetToken()) == 0 { return ctx, nil }` is what happens when
//!    "absent" gets its own branch: the absent case fails OPEN while the
//!    configured-but-weak case is refused. Here [`is_unset_credential`] is the
//!    single predicate, and
//!    `empty_and_the_sentinel_produce_byte_identical_refusals` drives every
//!    row of [`zeroship_secret_policy::PLATFORM_SECRETS`] at both values and requires
//!    the two messages to be equal, so the identity is measured rather than
//!    asserted in prose.
//! 3. **Refuse to boot**, with a banner naming the KEY, the FILE, and the
//!    exact remediation command. Not a log line among a thousand log lines.
//! 4. **The dev escape keys on the BUILD PROFILE** ([`BuildProfile::current`],
//!    which is `cfg!(debug_assertions)`) and on nothing else. It is not a flag
//!    and not an environment variable: `--dev-insecure` and
//!    `ZEROSHIP_DEV_INSECURE` were both deleted from this tree, and
//!    `crates/auth/src/config.rs` carries a test that refuses to let either
//!    come back. A production operator cannot set a build profile from a
//!    deployment file, which is exactly the property "never a plain
//!    environment variable a production operator might set" asks for.
//! 5. **Per subsystem.** A single-VPS deployer who never enables the migration
//!    service is never blocked on the migration service's credential.
//!    [`SubsystemCredential::enabled`] is that switch, and a disabled
//!    subsystem's credential is SKIPPED and counted as skipped, not silently
//!    passed.
//!
//! # `--check-config` exits non-zero, in every build
//!
//! [`CredentialPosture::verdict`] returns [`CredentialVerdict::Refuse`] for a
//! weak credential on a dry run REGARDLESS of profile, and the reason is
//! measured rather than stylistic.
//! `docs/proposals/2026-08-20-metering-transport-not-configured.md` records
//! that `--check-config` had been truthfully reporting
//! `usage_stream_configured=false` for 43 days while `deploy-remote.sh` read
//! only the exit code and threw stdout away, and nothing was metered for the
//! whole of that time. A posture field that is merely REPORTED is not a gate.
//! The exit code is the only channel that reaches a deploy decision, so the
//! posture is published in [`crate::config::CheckConfigReport`] AND carried by
//! the exit code.
//!
//! Making the dry run profile-independent is also what keeps this gate
//! testable: `--check-config` on a DEBUG binary exercises the production arm,
//! so proving the refusal does not require a release build of every service.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use zeroship_secret_policy::{is_unset_credential, REMEDIATION_COMMAND};

use crate::config::names::Secret;
use crate::config::source::ConfigSource;

/// Whether THIS PROCESS booted on the development escape.
///
/// A process-wide flag rather than a field on each service's state struct, and
/// that is the right shape rather than a shortcut: it is a property of the
/// PROCESS - decided once, before any state object exists, by a `main` that
/// chose to continue - and the five services' readiness handlers hang off five
/// unrelated state types. Threading one boolean through all five would couple
/// each of them to a concern none of them owns, and would give five places for
/// the wiring to be forgotten in.
///
/// Write-once at boot, read on every `/readyz`. `Relaxed` is sufficient: the
/// write happens-before the listener binds, so no probe can observe the
/// pre-write value.
static DEV_ESCAPE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Record that this process booted on [`CredentialVerdict::DevEscape`].
///
/// Called only from a `main` that has already printed the banner. There is no
/// unmark: a process cannot stop having started on a placeholder credential.
pub fn mark_dev_escape_active() {
    DEV_ESCAPE_ACTIVE.store(true, Ordering::Relaxed);
}

/// True when this process booted on an unconfigured service credential.
///
/// `/readyz` returns 503 while this is true. THAT IS THE POINT, and it is not a
/// warning dressed up as one: a process holding a placeholder credential must
/// not be routed traffic, and an orchestrator that reads readiness is the only
/// consumer that acts on the answer without a human in the loop. Gitaly's
/// mistake was making a Prometheus label the only signal - a value nobody
/// alerts on and nothing acts on.
///
/// It costs local development nothing, because local development never takes
/// this path: `zeroship dev init` generates real material and
/// `deploy/compose/docker-compose.yml` requires every platform secret with
/// `${VAR:?run zeroship dev init}`, which refuses to interpolate when unset.
/// The escape exists for `cargo run` on an unprovisioned checkout, where
/// nothing probes `/readyz`.
///
/// It deliberately does NOT say WHICH credential. `/readyz` is unauthenticated;
/// see [`CredentialPosture::summary`].
#[must_use]
pub fn dev_escape_active() -> bool {
    DEV_ESCAPE_ACTIVE.load(Ordering::Relaxed)
}

/// Which build this binary is.
///
/// The ONLY dev signal this gate honours. It is decided at compile time by
/// [`BuildProfile::current`] and there is deliberately no way to override it
/// from a flag, an environment variable or a config file: an escape hatch a
/// production operator can reach from a deployment file is not an escape hatch,
/// it is a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildProfile {
    /// `cargo build` / `cargo run` / `cargo test` -- `debug_assertions` on.
    Development,
    /// `cargo build --release`, which is what `deploy/Dockerfile` builds and
    /// what every published image runs.
    Production,
}

impl BuildProfile {
    /// This binary's profile.
    ///
    /// `cfg!(debug_assertions)` rather than `#[cfg]` so both arms compile in
    /// both builds and the production arm is reachable from a debug test.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(debug_assertions) {
            Self::Development
        } else {
            Self::Production
        }
    }
}

impl fmt::Display for BuildProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Development => f.write_str("development"),
            Self::Production => f.write_str("production"),
        }
    }
}

/// One credential a named subsystem cannot run without.
///
/// Built by a service's `main` immediately after config resolution, one per
/// credential, and handed to [`audit_credentials`].
#[derive(Debug)]
pub struct SubsystemCredential<'a> {
    /// The subsystem the credential belongs to, in the operator's vocabulary
    /// (`worker-dispatch`, `route-sync`, `app-migrations`). Named in the banner
    /// so a deployer who does not run that subsystem knows what to turn off
    /// instead of what to provision.
    pub subsystem: &'static str,
    /// Whether THIS process enabled the subsystem.
    ///
    /// A disabled subsystem's credential is skipped. This is the per-subsystem
    /// property: a single-VPS deployer is never blocked on a credential for a
    /// service they never enabled.
    pub enabled: bool,
    /// The operator-facing spelling, the same string the validator interpolates
    /// (`ZEROSHIP_WORKER_KEY / --worker-key-file`).
    pub label: &'static str,
    /// The resolved secret. `None` material on a `--check-config` run for a
    /// source that would need I/O; see [`audit_credentials`].
    pub secret: &'a Secret<String>,
    /// The strength validator this credential's binary runs at boot. Taken as a
    /// function pointer so the audit runs the SAME check the service runs, not
    /// a second spelling of it.
    pub validate: fn(&str, &str) -> Result<(), String>,
}

/// What one enabled credential was found to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeakCredential {
    /// The subsystem that cannot run.
    pub subsystem: &'static str,
    /// The operator-facing variable spelling.
    pub label: &'static str,
    /// The validator's own message, unchanged.
    pub message: String,
    /// True when the credential was empty or the sentinel, as opposed to
    /// present but too weak. Reported so the banner can say which, and NEVER
    /// consulted to decide the outcome -- the outcome is the same either way.
    pub unset: bool,
}

/// The result of auditing one service's credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialPosture {
    /// Credentials whose subsystem is enabled and which were judged.
    checked: usize,
    /// Credentials skipped because their subsystem is disabled.
    skipped: usize,
    /// Credentials skipped because this run is a dry run and the source needs
    /// I/O to read. There is nothing to judge; judging the reference TEXT
    /// instead of the material is the mistake `validate_secret_material`
    /// already removed.
    unread: usize,
    /// Every enabled credential that failed, in declaration order.
    weak: Vec<WeakCredential>,
}

impl CredentialPosture {
    /// Credentials this audit actually ruled on.
    #[must_use]
    pub const fn checked(&self) -> usize {
        self.checked
    }

    /// Credentials skipped because their subsystem is off.
    #[must_use]
    pub const fn skipped(&self) -> usize {
        self.skipped
    }

    /// Credentials skipped because a dry run did not read their material.
    #[must_use]
    pub const fn unread(&self) -> usize {
        self.unread
    }

    /// Every enabled credential that failed.
    #[must_use]
    pub fn weak(&self) -> &[WeakCredential] {
        &self.weak
    }

    /// True when every enabled credential passed.
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        self.weak.is_empty()
    }

    /// The one-word posture published in `--check-config`, `zeroship config
    /// check` and `/readyz`.
    ///
    /// THREE values, and the third is the one this report would otherwise lie
    /// with. `--check-config` deliberately does not open a `-file` secret - a
    /// dry run establishes the SOURCE of every credential without I/O - so a
    /// dry run over a deployment that supplies its credentials as files judges
    /// NOTHING. Reporting `configured` there would be a green built entirely out
    /// of unread material, which is precisely the vacuous pass this repository
    /// hunts in its gates. `unverified` says what actually happened: the
    /// credentials are configured, and this run did not read them.
    ///
    /// Deliberately does NOT name the offending variable. `/readyz` is
    /// unauthenticated, and "this host runs on the default key, here is which
    /// one" is precisely the sentence an attacker wants. The variable name goes
    /// in the banner, which goes to the operator's terminal and log.
    #[must_use]
    pub const fn summary(&self) -> &'static str {
        if !self.weak.is_empty() {
            "weak"
        } else if self.checked == 0 && self.unread > 0 {
            "unverified"
        } else {
            "configured"
        }
    }

    /// What this process must do about the posture.
    ///
    /// `dry_run` is the `--check-config` flag. A dry run is REFUSED in every
    /// build: it asks "is this configuration deployable", and the answer for a
    /// placeholder credential is no whatever the asking binary was compiled
    /// with. See the module docs for the 43-day metering measurement that makes
    /// the exit code, and not the report field, the thing that gates a deploy.
    #[must_use]
    pub fn verdict(&self, profile: BuildProfile, dry_run: bool) -> CredentialVerdict {
        if self.weak.is_empty() {
            CredentialVerdict::Proceed
        } else if dry_run || profile == BuildProfile::Production {
            CredentialVerdict::Refuse
        } else {
            CredentialVerdict::DevEscape
        }
    }

    /// The operator-facing banner for a non-[`CredentialVerdict::Proceed`]
    /// verdict.
    ///
    /// Names the KEY, the FILE, and the exact remediation command, per the
    /// Superset fix. "The file" is reported from what was MEASURED: the overlay
    /// this process actually loaded, plus the supply tier each credential
    /// actually came from. It is never a guessed path -- an operator told to
    /// edit a file the process does not read is worse served than one told
    /// nothing.
    ///
    /// Returns `None` for [`CredentialVerdict::Proceed`], so a caller cannot
    /// print a refusal banner for a healthy configuration.
    #[must_use]
    pub fn banner(
        &self,
        binary: &str,
        overlay: &ConfigSource,
        verdict: CredentialVerdict,
    ) -> Option<String> {
        use std::fmt::Write as _;

        let headline = match verdict {
            CredentialVerdict::Proceed => return None,
            CredentialVerdict::Refuse => {
                format!("{binary} REFUSES TO START: service credential not configured")
            }
            CredentialVerdict::DevEscape => format!(
                "{binary} IS RUNNING ON AN UNCONFIGURED SERVICE CREDENTIAL \
                 (development build only)"
            ),
        };

        let rule = "=".repeat(78);
        let thin = "-".repeat(78);
        let mut out = format!("\n{rule}\n  {headline}\n{thin}\n");
        for weak in &self.weak {
            let _ = writeln!(out, "  subsystem    {}", weak.subsystem);
            let _ = writeln!(out, "  key          {}", weak.label);
            let _ = writeln!(out, "  problem      {}", weak.message);
        }
        let _ = writeln!(out, "  config file  {overlay}");
        let _ = writeln!(out, "  remediation  {REMEDIATION_COMMAND}");
        if verdict == CredentialVerdict::DevEscape {
            out.push_str(
                "  note         this build is a DEVELOPMENT build \
                 (cfg!(debug_assertions)).\n\
                 \x20              A release build refuses to start on this configuration,\n\
                 \x20              and /readyz reports NOT ready while the escape is active.\n",
            );
        }
        out.push_str(&rule);
        out.push('\n');
        Some(out)
    }
}

/// What a [`CredentialPosture`] requires of the process holding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialVerdict {
    /// Every enabled credential passed. Boot normally.
    Proceed,
    /// Print the banner and exit non-zero.
    Refuse,
    /// Print the banner and continue. Reachable ONLY from a development build
    /// on a real boot; a `--check-config` run never takes this arm.
    DevEscape,
}

/// Judge one service's credentials.
///
/// Runs each enabled credential's own validator on its own material, in
/// declaration order, and collects EVERY failure rather than stopping at the
/// first. An operator restarting a service once per missing credential is how a
/// five-minute fix becomes an hour.
///
/// The three-way material handling matches
/// [`crate::config::validate_secret_material`]: real material is validated;
/// material a dry run deliberately did not read is counted as `unread` and not
/// judged; an unsupplied secret is validated as `""`, which is how each
/// validator produces its own "is not configured" message rather than a generic
/// one.
#[must_use]
pub fn audit_credentials(credentials: &[SubsystemCredential<'_>]) -> CredentialPosture {
    let mut posture = CredentialPosture {
        checked: 0,
        skipped: 0,
        unread: 0,
        weak: Vec::new(),
    };

    for credential in credentials {
        if !credential.enabled {
            posture.skipped += 1;
            continue;
        }
        let material = match credential.secret.expose_secret() {
            Some(material) => material.as_str(),
            None if credential.secret.is_configured() => {
                posture.unread += 1;
                continue;
            }
            None => "",
        };
        posture.checked += 1;
        if let Err(message) = (credential.validate)(credential.label, material) {
            posture.weak.push(WeakCredential {
                subsystem: credential.subsystem,
                label: credential.label,
                unset: is_unset_credential(material),
                message,
            });
        }
    }

    posture
}

#[cfg(test)]
mod tests {
    use zeroship_secret_policy::{
        is_unset_credential, require_nonempty, unset_credential_message, validate_worker_key,
        MIN_SECRET_BYTES, PLATFORM_SECRETS, SERVICE_CREDENTIAL_SENTINEL,
    };

    use super::{
        audit_credentials, BuildProfile, CredentialPosture, CredentialVerdict, SubsystemCredential,
    };
    use crate::config::names::{Secret, SourceKind};
    use crate::config::source::ConfigSource;

    const LABEL: &str = "ZEROSHIP_WORKER_KEY / --worker-key-file";
    const STRONG: &str = "0123456789abcdef0123456789abcdef";

    fn supplied(value: &str) -> Secret<String> {
        Secret::supplied(SourceKind::Env, Some(value.to_owned()))
    }

    fn one(secret: &Secret<String>, enabled: bool) -> Vec<SubsystemCredential<'_>> {
        vec![SubsystemCredential {
            subsystem: "worker-dispatch",
            enabled,
            label: LABEL,
            secret,
            validate: validate_worker_key,
        }]
    }

    /// THE PROPERTY THE WHOLE GATE RESTS ON, measured rather than claimed.
    ///
    /// Gitaly's `if len(conf.GetToken()) == 0 { return ctx, nil }` is what a
    /// separate branch for "absent" becomes. Every row of the real table is
    /// driven at BOTH values and the two messages must be the same bytes -- not
    /// "both are errors", which a special case would also satisfy.
    #[test]
    fn empty_and_the_sentinel_produce_byte_identical_refusals() {
        assert!(
            !PLATFORM_SECRETS.is_empty(),
            "an empty table is the vacuous case; this test would rule on nothing"
        );
        for secret in PLATFORM_SECRETS {
            let empty = secret.validate("").expect_err("empty is refused");
            let sentinel = secret
                .validate(SERVICE_CREDENTIAL_SENTINEL)
                .expect_err("the sentinel is refused");
            assert_eq!(
                empty, sentinel,
                "{} treats empty and the sentinel differently, which is the Gitaly defect",
                secret.env
            );
        }
    }

    /// The one-variable control for the test above: the SAME rows accept a
    /// value that differs from the sentinel only in being real key material, so
    /// the equality above is not passing because everything is refused.
    #[test]
    fn a_real_credential_is_accepted_by_the_same_rows() {
        let hex = "ab".repeat(MIN_SECRET_BYTES);
        for secret in PLATFORM_SECRETS {
            secret
                .validate(&hex)
                .unwrap_or_else(|e| panic!("{} must accept real material: {e}", secret.env));
        }
    }

    /// The sentinel is 30 bytes and the raw floor is 32, so a reader could
    /// conclude the length rule catches it and the sentinel branch is
    /// decoration. It does not: the refusal is the UNSET one, in its own words,
    /// and this pins that the sentinel is caught for being the sentinel.
    #[test]
    fn the_sentinel_is_refused_as_unset_not_as_too_short() {
        assert!(
            SERVICE_CREDENTIAL_SENTINEL.len() < MIN_SECRET_BYTES,
            "the premise of this test is that the sentinel is under the floor"
        );
        let message = validate_worker_key(LABEL, SERVICE_CREDENTIAL_SENTINEL)
            .expect_err("the sentinel is refused");
        assert_eq!(message, unset_credential_message(LABEL));
        assert!(!message.contains("too short"), "{message}");

        // The one-variable partner: a 30-byte value that is NOT the sentinel is
        // refused for being too short, which is the branch this test asserts the
        // sentinel does not take.
        let short = "a".repeat(SERVICE_CREDENTIAL_SENTINEL.len());
        assert!(validate_worker_key(LABEL, &short)
            .expect_err("30 bytes is under the floor")
            .contains("too short"));
    }

    /// `Unrestricted` rows have NO length floor, so the sentinel branch is the
    /// only thing that can refuse a placeholder there. This is the case the gate
    /// exists for.
    #[test]
    fn a_credential_with_no_length_floor_still_refuses_the_sentinel() {
        assert_eq!(
            require_nonempty(LABEL, SERVICE_CREDENTIAL_SENTINEL)
                .expect_err("the sentinel is refused even with no floor"),
            unset_credential_message(LABEL)
        );
        // The one-variable partner: one byte of real material passes the same
        // validator, so the refusal is about the sentinel and not about length.
        require_nonempty(LABEL, "x").expect("a floorless credential accepts any real value");
    }

    #[test]
    fn whitespace_around_the_sentinel_does_not_defeat_it() {
        assert!(is_unset_credential(&format!(" {SERVICE_CREDENTIAL_SENTINEL}\n")));
        assert!(is_unset_credential("   "));
        assert!(!is_unset_credential(STRONG));
    }

    #[test]
    fn a_disabled_subsystem_is_skipped_not_judged() {
        let secret = supplied(SERVICE_CREDENTIAL_SENTINEL);
        let posture = audit_credentials(&one(&secret, false));
        assert!(posture.is_ok());
        assert_eq!((posture.checked(), posture.skipped()), (0, 1));

        // The one-variable partner: the SAME sentinel with the subsystem
        // ENABLED is weak, so the skip above is about `enabled` and nothing
        // else.
        let enabled = audit_credentials(&one(&secret, true));
        assert!(!enabled.is_ok());
        assert_eq!((enabled.checked(), enabled.skipped()), (1, 0));
    }

    /// A dry run must not judge a credential it deliberately did not read -
    /// AND must not report the result as a clean bill of health.
    ///
    /// THE DEFECT THIS PINS, found by running the real binary: a
    /// `--check-config` run supplying every gateway credential through
    /// `--<name>-file` printed `service_credentials = configured` alongside
    /// `service_credentials_checked = 0` and `service_credentials_unread = 4`.
    /// The counts were right and the word was wrong, and the word is what a
    /// reader takes away.
    #[test]
    fn a_dry_run_does_not_judge_material_it_did_not_read() {
        let unread: Secret<String> = Secret::supplied(SourceKind::CliFile, None);
        let posture = audit_credentials(&one(&unread, true));
        assert!(posture.is_ok());
        assert_eq!((posture.checked(), posture.unread()), (0, 1));
        assert_eq!(
            posture.summary(),
            "unverified",
            "a posture built from zero readings must not read as configured"
        );

        // The one-variable partner: the SAME source with material present is
        // `configured`, so `unverified` is about the reading and not about the
        // source tier.
        let read = Secret::supplied(SourceKind::CliFile, Some(STRONG.to_owned()));
        let judged = audit_credentials(&one(&read, true));
        assert_eq!((judged.checked(), judged.unread()), (1, 0));
        assert_eq!(judged.summary(), "configured");
    }

    /// A dry run that reads SOME credentials and not others is `configured`,
    /// not `unverified`: something really was ruled on.
    #[test]
    fn a_partly_read_dry_run_reports_configured_with_the_counts_alongside() {
        let read = supplied(STRONG);
        let unread: Secret<String> = Secret::supplied(SourceKind::CliFile, None);
        let credentials = vec![
            SubsystemCredential {
                subsystem: "worker-dispatch",
                enabled: true,
                label: LABEL,
                secret: &read,
                validate: validate_worker_key,
            },
            SubsystemCredential {
                subsystem: "route-sync",
                enabled: true,
                label: "ZEROSHIP_CONTROL_KEY",
                secret: &unread,
                validate: require_nonempty,
            },
        ];
        let posture = audit_credentials(&credentials);
        assert_eq!((posture.checked(), posture.unread()), (1, 1));
        assert_eq!(posture.summary(), "configured");
    }

    #[test]
    fn an_unsupplied_secret_is_judged_as_empty() {
        let absent: Secret<String> = Secret::absent();
        let posture = audit_credentials(&one(&absent, true));
        assert_eq!(posture.weak().len(), 1);
        assert!(posture.weak()[0].unset);
    }

    /// Empty and sentinel must reach the SAME verdict from the SAME profile.
    #[test]
    fn empty_and_sentinel_reach_the_same_verdict() {
        for value in ["", SERVICE_CREDENTIAL_SENTINEL] {
            let secret = supplied(value);
            let posture = audit_credentials(&one(&secret, true));
            assert_eq!(
                posture.verdict(BuildProfile::Production, false),
                CredentialVerdict::Refuse,
                "value {value:?}"
            );
            assert_eq!(
                posture.verdict(BuildProfile::Development, true),
                CredentialVerdict::Refuse,
                "a dry run refuses in every build; value {value:?}"
            );
            assert_eq!(
                posture.verdict(BuildProfile::Development, false),
                CredentialVerdict::DevEscape,
                "value {value:?}"
            );
        }
    }

    /// THE ONE-VARIABLE CONTROL FOR THE WHOLE GATE. Only the credential
    /// changes; a correctly configured one proceeds on every profile and on the
    /// dry run.
    #[test]
    fn a_correctly_configured_credential_proceeds_everywhere() {
        let secret = supplied(STRONG);
        let posture = audit_credentials(&one(&secret, true));
        assert!(posture.is_ok());
        assert_eq!(posture.summary(), "configured");
        for profile in [BuildProfile::Development, BuildProfile::Production] {
            for dry_run in [false, true] {
                assert_eq!(
                    posture.verdict(profile, dry_run),
                    CredentialVerdict::Proceed,
                    "{profile} dry_run={dry_run}"
                );
            }
        }
        assert!(posture
            .banner("zeroship-gate", &ConfigSource::None, CredentialVerdict::Proceed)
            .is_none());
    }

    #[test]
    fn the_banner_names_the_key_the_file_and_the_command() {
        let secret = supplied(SERVICE_CREDENTIAL_SENTINEL);
        let posture = audit_credentials(&one(&secret, true));
        let overlay = ConfigSource::Explicit("/etc/zeroship/zeroship.toml".into());
        let banner = posture
            .banner("zeroship-gate", &overlay, CredentialVerdict::Refuse)
            .expect("a weak posture has a banner");
        assert!(banner.contains("ZEROSHIP_WORKER_KEY"), "{banner}");
        assert!(banner.contains("/etc/zeroship/zeroship.toml"), "{banner}");
        assert!(banner.contains("zeroship dev init"), "{banner}");
        assert!(banner.contains("worker-dispatch"), "{banner}");
        assert!(banner.contains("REFUSES TO START"), "{banner}");
        assert!(banner.contains(SERVICE_CREDENTIAL_SENTINEL), "{banner}");
    }

    #[test]
    fn the_dev_escape_banner_says_a_release_build_would_refuse() {
        let secret = supplied(SERVICE_CREDENTIAL_SENTINEL);
        let posture = audit_credentials(&one(&secret, true));
        let banner = posture
            .banner("zeroship-gate", &ConfigSource::None, CredentialVerdict::DevEscape)
            .expect("a weak posture has a banner");
        assert!(banner.contains("debug_assertions"), "{banner}");
        assert!(banner.contains("release build refuses"), "{banner}");
        assert!(banner.contains("/readyz"), "{banner}");
    }

    #[test]
    fn the_posture_summary_never_names_the_variable() {
        // /readyz and the check-config posture field are unauthenticated or
        // widely copied; the variable name belongs in the banner only.
        let secret = supplied(SERVICE_CREDENTIAL_SENTINEL);
        let posture = audit_credentials(&one(&secret, true));
        assert_eq!(posture.summary(), "weak");
        assert!(!posture.summary().contains("ZEROSHIP"));
    }

    #[test]
    fn every_weak_credential_is_reported_not_only_the_first() {
        let a = supplied("");
        let b = supplied(SERVICE_CREDENTIAL_SENTINEL);
        let credentials = vec![
            SubsystemCredential {
                subsystem: "route-sync",
                enabled: true,
                label: "ZEROSHIP_CONTROL_KEY",
                secret: &a,
                validate: require_nonempty,
            },
            SubsystemCredential {
                subsystem: "worker-dispatch",
                enabled: true,
                label: LABEL,
                secret: &b,
                validate: validate_worker_key,
            },
        ];
        let posture = audit_credentials(&credentials);
        assert_eq!(posture.weak().len(), 2, "{posture:?}");
        assert_eq!(posture.checked(), 2);
    }

    #[test]
    fn the_current_profile_matches_the_build() {
        let expected = if cfg!(debug_assertions) {
            BuildProfile::Development
        } else {
            BuildProfile::Production
        };
        assert_eq!(BuildProfile::current(), expected);
    }

    /// A posture with nothing in it must not read as "checked and clean".
    #[test]
    fn an_audit_of_nothing_reports_zero_checked() {
        let posture: CredentialPosture = audit_credentials(&[]);
        assert_eq!(posture.checked(), 0);
        assert!(posture.is_ok());
    }
}
