//! Argon2id roundtrip + enumeration-defense timing test.

use std::time::Instant;
use zeroship_auth::identity::password;

#[test]
fn hash_verify_roundtrip() {
    let phc = password::hash("correct-horse-battery-staple").expect("hash");
    assert!(password::verify("correct-horse-battery-staple", &phc).expect("verify"));
    assert!(!password::verify("wrong-password", &phc).expect("verify"));
}

#[test]
fn dummy_hash_is_constant_time_within_tolerance() {
    // Warm the dummy hash so first-call init isn't counted.
    let _ = password::dummy_hash();

    let real_phc = password::hash("real-password-here").expect("hash");

    let t_real = {
        let t0 = Instant::now();
        let _ = password::verify("wrong-password", &real_phc).expect("verify");
        t0.elapsed()
    };
    let t_dummy = {
        let t0 = Instant::now();
        let _ = password::verify_against_dummy("any-password").expect("verify");
        t0.elapsed()
    };

    let ratio = t_dummy.as_secs_f64() / t_real.as_secs_f64();
    assert!(
        ratio > 0.5 && ratio < 2.0,
        "dummy/real timing ratio = {ratio}, expected ~1.0 (got real={t_real:?}, dummy={t_dummy:?})"
    );
}
