//! Minimal DER walker for extracting raw private-key seeds from
//! aws-lc-rs-generated PKCS#8 blobs.
//!
//! aws-lc-rs's high-level keygen for ECDSA / ECDH / Ed25519 hands back
//! a `Document` containing PKCS#8 PrivateKeyInfo bytes; the raw scalar
//! is buried inside (it isn't surfaced as a typed accessor). We need
//! the scalar for JWK export of generated keys.
//!
//! Layouts handled:
//!
//! - **PKCS#8 PrivateKeyInfo** (RFC 5208):
//!   ```text
//!   PrivateKeyInfo ::= SEQUENCE {
//!     version                   INTEGER (0),
//!     privateKeyAlgorithm       AlgorithmIdentifier,
//!     privateKey                OCTET STRING -- inner key encoding
//!   }
//!   ```
//! - **EC SEC1 ECPrivateKey** (RFC 5915), inside the OCTET STRING:
//!   ```text
//!   ECPrivateKey ::= SEQUENCE {
//!     version                   INTEGER (1),
//!     privateKey                OCTET STRING -- raw d scalar (curve-order bytes)
//!     parameters            [0] EXPLICIT ECParameters OPTIONAL,
//!     publicKey             [1] EXPLICIT BIT STRING   OPTIONAL
//!   }
//!   ```
//! - **CFRG OneAsymmetricKey** (RFC 8410), inside the OCTET STRING for
//!   Ed25519 / X25519: a CurvePrivateKey ::= OCTET STRING wrapping the
//!   raw 32-byte seed. So the inner is `04 20 <32 bytes>`.
//!
//! No general-purpose ASN.1 lib needed. Walker rejects any input
//! that doesn't match the expected shape so we don't accidentally
//! interpret garbage as a seed.

#![allow(dead_code)]

/// Walk a PKCS#8 PrivateKeyInfo and return (algorithm-identifier-OID-bytes,
/// privateKey-OCTET-STRING-contents). The OID bytes returned are the
/// raw OID octets (without the OBJECT IDENTIFIER tag/length).
///
/// Accepts PKCS#8 v1 (RFC 5208 — version 0) and v2 (RFC 5958 — version
/// 1, which adds an `attributes [0]` and `publicKey [1]` implicit). The
/// `[0]` and `[1]` tags after the privateKey OCTET STRING are tolerated
/// and skipped.
///
/// On any structural mismatch returns `None`.
pub fn parse_pkcs8_private_key_info(input: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (top, rest) = read_tlv(input)?;
    if !rest.is_empty() {
        return None;
    }
    if top.tag != 0x30 {
        return None;
    }
    let body = top.value;

    // version INTEGER 0 (RFC 5208) or 1 (RFC 5958)
    let (ver, body) = read_tlv(body)?;
    if ver.tag != 0x02 || (ver.value != [0x00] && ver.value != [0x01]) {
        return None;
    }

    // privateKeyAlgorithm AlgorithmIdentifier ::= SEQUENCE { algorithm OID, params ANY }
    let (alg_id, body) = read_tlv(body)?;
    if alg_id.tag != 0x30 {
        return None;
    }
    let (oid_tlv, _) = read_tlv(alg_id.value)?;
    if oid_tlv.tag != 0x06 {
        return None;
    }
    let oid = oid_tlv.value.to_vec();

    // privateKey OCTET STRING. Trailing v2 fields ([0] attributes, [1]
    // publicKey) are not consumed — caller doesn't need them.
    let (priv_oct, _) = read_tlv(body)?;
    if priv_oct.tag != 0x04 {
        return None;
    }

    Some((oid, priv_oct.value.to_vec()))
}

/// Walk a SEC1 ECPrivateKey and return the raw scalar `d` (left-padded
/// to `expected_len` bytes — for P-256 = 32, P-384 = 48, P-521 = 66).
///
/// `input` should be the OCTET STRING contents from PKCS#8 (not the
/// outer PKCS#8 wrapper itself).
pub fn parse_ec_private_key_scalar(input: &[u8], expected_len: usize) -> Option<Vec<u8>> {
    let (top, rest) = read_tlv(input)?;
    if !rest.is_empty() {
        return None;
    }
    if top.tag != 0x30 {
        return None;
    }
    let body = top.value;

    // version INTEGER 1
    let (ver, body) = read_tlv(body)?;
    if ver.tag != 0x02 || ver.value != [0x01] {
        return None;
    }

    // privateKey OCTET STRING — raw d
    let (priv_tlv, _) = read_tlv(body)?;
    if priv_tlv.tag != 0x04 {
        return None;
    }
    let raw = priv_tlv.value;
    // RFC 5915 §3 says d MUST be encoded with leading zeros, so it's
    // exactly the curve-order byte length. Some encoders strip leading
    // zeros (RFC violation but seen in the wild) — left-pad if shorter.
    if raw.len() == expected_len {
        Some(raw.to_vec())
    } else if raw.len() < expected_len {
        let mut out = vec![0u8; expected_len];
        out[expected_len - raw.len()..].copy_from_slice(raw);
        Some(out)
    } else {
        // Some encoders prefix with a single 0 if the high bit would
        // otherwise indicate negative — strip it.
        if raw.len() == expected_len + 1 && raw[0] == 0 {
            Some(raw[1..].to_vec())
        } else {
            None
        }
    }
}

/// Walk a CFRG CurvePrivateKey ::= OCTET STRING wrapping a raw 32-byte
/// seed (Ed25519 / X25519 per RFC 8410 §7).
///
/// `input` should be the OCTET STRING contents from PKCS#8.
pub fn parse_cfrg_private_key_seed(input: &[u8]) -> Option<[u8; 32]> {
    let (oct, rest) = read_tlv(input)?;
    if !rest.is_empty() {
        return None;
    }
    if oct.tag != 0x04 || oct.value.len() != 32 {
        return None;
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(oct.value);
    Some(seed)
}

/// One-shot: extract the raw EC scalar from a PKCS#8 blob produced by
/// aws-lc-rs's ECDSA/ECDH keygen. Returns `None` on any malformation.
pub fn extract_ec_raw_d(pkcs8: &[u8], expected_len: usize) -> Option<Vec<u8>> {
    let (_oid, inner) = parse_pkcs8_private_key_info(pkcs8)?;
    parse_ec_private_key_scalar(&inner, expected_len)
}

/// One-shot: extract the raw 32-byte seed from a PKCS#8 blob produced
/// by aws-lc-rs's Ed25519 keygen.
pub fn extract_cfrg_raw_seed(pkcs8: &[u8]) -> Option<[u8; 32]> {
    let (_oid, inner) = parse_pkcs8_private_key_info(pkcs8)?;
    parse_cfrg_private_key_seed(&inner)
}

// -----------------------------------------------------------------------------
// Internals
// -----------------------------------------------------------------------------

struct Tlv<'a> {
    tag: u8,
    value: &'a [u8],
}

/// Read a single DER TLV. Returns the parsed TLV and the trailing
/// (un-consumed) bytes. None on any malformation.
fn read_tlv(input: &[u8]) -> Option<(Tlv<'_>, &[u8])> {
    if input.is_empty() {
        return None;
    }
    let tag = input[0];
    if input.len() < 2 {
        return None;
    }
    let first_len = input[1];
    let (len, off) = if first_len & 0x80 == 0 {
        (first_len as usize, 2)
    } else {
        let n = (first_len & 0x7f) as usize;
        if n == 0 || n > 4 {
            return None; // indefinite or absurdly long
        }
        if input.len() < 2 + n {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | (input[2 + i] as usize);
        }
        (len, 2 + n)
    };
    if input.len() < off + len {
        return None;
    }
    let value = &input[off..off + len];
    let rest = &input[off + len..];
    Some((Tlv { tag, value }, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PKCS#8 P-256 private key (test vector — random d, not real).
    /// Generated externally from a known scalar so we can verify the
    /// walker pulls it back out. Hex-decoded inline.
    fn h(s: &str) -> Vec<u8> {
        s.chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .as_bytes()
            .chunks(2)
            .map(|c| {
                u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap()
            })
            .collect()
    }

    #[test]
    fn read_simple_tlv() {
        let data = [0x02u8, 0x01, 0x07, 0xff];
        let (tlv, rest) = read_tlv(&data).unwrap();
        assert_eq!(tlv.tag, 0x02);
        assert_eq!(tlv.value, &[0x07]);
        assert_eq!(rest, &[0xff]);
    }

    #[test]
    fn read_long_form_length() {
        let mut data = vec![0x04, 0x82, 0x01, 0x00];
        data.extend(std::iter::repeat(0xab).take(0x100));
        let (tlv, rest) = read_tlv(&data).unwrap();
        assert_eq!(tlv.tag, 0x04);
        assert_eq!(tlv.value.len(), 0x100);
        assert!(rest.is_empty());
    }

    #[test]
    fn rejects_truncated_length() {
        let data = [0x02u8, 0x82];
        assert!(read_tlv(&data).is_none());
    }

    #[test]
    fn rejects_truncated_value() {
        let data = [0x02u8, 0x05, 0xff, 0xff];
        assert!(read_tlv(&data).is_none());
    }

    /// Round-trip an EC PKCS#8 produced by aws-lc-rs (real generated
    /// key). This is the truest end-to-end check.
    #[test]
    fn extract_ec_p256_round_trip_real_key() {
        let signing = &aws_lc_rs::signature::ECDSA_P256_SHA256_FIXED_SIGNING;
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let pkcs8 = aws_lc_rs::signature::EcdsaKeyPair::generate_pkcs8(signing, &rng).unwrap();
        let bytes = pkcs8.as_ref();
        let raw_d = extract_ec_raw_d(bytes, 32).expect("extract failed");
        assert_eq!(raw_d.len(), 32);
        // Raw scalar must be non-zero with overwhelming probability.
        assert!(raw_d.iter().any(|&b| b != 0));
    }

    #[test]
    fn extract_ec_p384_round_trip_real_key() {
        let signing = &aws_lc_rs::signature::ECDSA_P384_SHA384_FIXED_SIGNING;
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let pkcs8 = aws_lc_rs::signature::EcdsaKeyPair::generate_pkcs8(signing, &rng).unwrap();
        let bytes = pkcs8.as_ref();
        let raw_d = extract_ec_raw_d(bytes, 48).expect("extract failed");
        assert_eq!(raw_d.len(), 48);
    }

    #[test]
    fn extract_ec_p521_round_trip_real_key() {
        let signing = &aws_lc_rs::signature::ECDSA_P521_SHA512_FIXED_SIGNING;
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let pkcs8 = aws_lc_rs::signature::EcdsaKeyPair::generate_pkcs8(signing, &rng).unwrap();
        let bytes = pkcs8.as_ref();
        let raw_d = extract_ec_raw_d(bytes, 66).expect("extract failed");
        assert_eq!(raw_d.len(), 66);
    }

    /// Walker should pull the scalar from a hand-built PKCS#8 too.
    #[test]
    fn extract_ec_p256_round_trip_synthetic() {
        let raw_d: [u8; 32] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c,
            0x1d, 0x1e, 0x1f, 0x20,
        ];
        // Minimal SEC1 ECPrivateKey: version 1 + d (no parameters / publicKey).
        let mut sec1 = Vec::new();
        sec1.extend_from_slice(&[0x02, 0x01, 0x01]); // version
        sec1.push(0x04);
        sec1.push(raw_d.len() as u8);
        sec1.extend_from_slice(&raw_d);
        let mut sec1_seq = vec![0x30];
        sec1_seq.push(sec1.len() as u8);
        sec1_seq.extend_from_slice(&sec1);
        // Wrap in PKCS#8.
        // P-256 OID: 1.2.840.10045.3.1.7 / ecPublicKey 1.2.840.10045.2.1
        let oid_curve = [0x06u8, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
        let pk_oid = [0x06u8, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
        let mut alg_id = Vec::new();
        alg_id.extend_from_slice(&pk_oid);
        alg_id.extend_from_slice(&oid_curve);
        let mut alg_id_seq = vec![0x30];
        alg_id_seq.push(alg_id.len() as u8);
        alg_id_seq.extend_from_slice(&alg_id);
        let mut priv_oct = vec![0x04];
        priv_oct.push(sec1_seq.len() as u8);
        priv_oct.extend_from_slice(&sec1_seq);
        let mut body = vec![0x02, 0x01, 0x00];
        body.extend_from_slice(&alg_id_seq);
        body.extend_from_slice(&priv_oct);
        // body.len here is small (~70 bytes), short-form length OK.
        assert!(body.len() < 128);
        let mut pkcs8 = vec![0x30];
        pkcs8.push(body.len() as u8);
        pkcs8.extend_from_slice(&body);

        let extracted = extract_ec_raw_d(&pkcs8, 32).unwrap();
        assert_eq!(extracted, raw_d.to_vec());
    }

    /// aws-lc-rs's Ed25519 keygen produces PKCS#8 v2 (version=1, with
    /// public key as a [1] implicit tagged BIT STRING after the
    /// privateKey OCTET STRING). The walker must accept it.
    #[test]
    fn extract_cfrg_seed_real_aws_lc_rs_key() {
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let pkcs8 = aws_lc_rs::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let bytes = pkcs8.as_ref();
        let seed = extract_cfrg_raw_seed(bytes).expect("Ed25519 PKCS#8 walk failed");
        assert_eq!(seed.len(), 32);
        assert!(seed.iter().any(|&b| b != 0));
        // Reload via aws-lc-rs and check that the public point we
        // derive matches what aws-lc-rs returned (sanity check that
        // the seed we extracted is the "right" one).
        let kp = aws_lc_rs::signature::Ed25519KeyPair::from_pkcs8(bytes).unwrap();
        use aws_lc_rs::signature::KeyPair as _;
        // Re-import seed via from_seed_unchecked + check public
        // equality.
        let kp2 = aws_lc_rs::signature::Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        assert_eq!(kp.public_key().as_ref(), kp2.public_key().as_ref());
    }

    #[test]
    fn extract_cfrg_seed_round_trip() {
        // Hand-build PKCS#8 with a CurvePrivateKey OCTET STRING
        // wrapping a 32-byte seed. RFC 8410 §7.
        let seed: [u8; 32] = [0x42; 32];

        // Inner: OCTET STRING tag, length 32, seed.
        let mut inner = vec![0x04u8, 32];
        inner.extend_from_slice(&seed);

        let oid_ed25519 = [0x06u8, 0x03, 0x2b, 0x65, 0x70]; // 1.3.101.112
        let mut alg_id = Vec::new();
        alg_id.extend_from_slice(&oid_ed25519);
        let mut alg_id_seq = vec![0x30];
        alg_id_seq.push(alg_id.len() as u8);
        alg_id_seq.extend_from_slice(&alg_id);

        let mut priv_oct = vec![0x04];
        priv_oct.push(inner.len() as u8);
        priv_oct.extend_from_slice(&inner);

        let mut body = vec![0x02, 0x01, 0x00];
        body.extend_from_slice(&alg_id_seq);
        body.extend_from_slice(&priv_oct);

        let mut pkcs8 = vec![0x30];
        pkcs8.push(body.len() as u8);
        pkcs8.extend_from_slice(&body);

        let got = extract_cfrg_raw_seed(&pkcs8).unwrap();
        assert_eq!(got, seed);
    }

    #[test]
    fn rejects_garbage() {
        let data = h("ffeeddcc");
        assert!(extract_ec_raw_d(&data, 32).is_none());
        assert!(extract_cfrg_raw_seed(&data).is_none());
    }
}
