//! Low-level RSA EVP_DigestSign / EVP_DigestVerify wrappers.
//!
//! The `aws_lc_rs::signature::RSA_PSS_*` / `RSA_PKCS1_SHA*` paths used
//! to be sufficient, but two cases need lower-level access:
//!
//! 1. **RSA-PSS variable saltLength** — aws-lc-rs hard-codes salt =
//!    digest length. The WebCrypto `RsaPssParams.saltLength` lets
//!    callers pick anything in 0..=(emLen − hLen − 2) per RFC 3447
//!    §9.1.1.
//! 2. **RSA-PKCS1-v1_5 / RSA-PSS with SHA-1** — aws-lc-rs only
//!    exposes `RSA_PKCS1_*_SHA1_FOR_LEGACY_USE_ONLY` for *verify*; no
//!    SHA-1 path for *sign*. The WebCrypto spec still requires SHA-1
//!    for RSASSA-PKCS1-v1_5 (§22) and RSA-PSS (§24).
//!
//! See `docs/proposals/webcrypto-native.md`.
//!
//! Surface kept minimal: a few safe entry points, all convert FFI
//! failures to `OpError::dom("OperationError", _)`.

#![allow(unsafe_code)]
#![allow(non_snake_case)]

use crate::crypto_native::key_material::HashAlgo;
use crate::state::OpError;

use aws_lc_sys as sys;

/// Sign `data` with `pkcs8_der` (RSA private key, PKCS#8) using
/// RSA-PSS / `hash` / MGF1(`hash`) and the requested salt length.
pub fn sign_with_salt(
    pkcs8_der: &[u8],
    hash: HashAlgo,
    data: &[u8],
    salt_len: i32,
) -> Result<Vec<u8>, OpError> {
    unsafe {
        let pkey = parse_pkcs8(pkcs8_der)?;
        let _guard = PkeyGuard(pkey);

        let md = md_for(hash)?;

        let md_ctx = sys::EVP_MD_CTX_new();
        if md_ctx.is_null() {
            return Err(op_err("EVP_MD_CTX_new"));
        }
        let _ctx_guard = MdCtxGuard(md_ctx);

        let mut pctx: *mut sys::EVP_PKEY_CTX = std::ptr::null_mut();
        if sys::EVP_DigestSignInit(md_ctx, &mut pctx, md, std::ptr::null_mut(), pkey) != 1 {
            return Err(op_err("EVP_DigestSignInit"));
        }
        // Caller passes the same pctx — we don't need to free it
        // separately; it's owned by md_ctx.
        if sys::EVP_PKEY_CTX_set_rsa_padding(pctx, sys::RSA_PKCS1_PSS_PADDING) != 1 {
            return Err(op_err("set_rsa_padding"));
        }
        if sys::EVP_PKEY_CTX_set_rsa_pss_saltlen(pctx, salt_len) != 1 {
            return Err(op_err("set_rsa_pss_saltlen"));
        }
        if sys::EVP_PKEY_CTX_set_rsa_mgf1_md(pctx, md) != 1 {
            return Err(op_err("set_rsa_mgf1_md"));
        }

        // Update + Final two-step (vs one-shot EVP_DigestSign): keeps
        // the path identical to a streaming signer if we ever need it.
        if sys::EVP_DigestSignUpdate(md_ctx, data.as_ptr() as *const _, data.len()) != 1 {
            return Err(op_err("EVP_DigestSignUpdate"));
        }
        let mut sig_len: usize = 0;
        if sys::EVP_DigestSignFinal(md_ctx, std::ptr::null_mut(), &mut sig_len) != 1 {
            return Err(op_err("EVP_DigestSignFinal(probe)"));
        }
        let mut sig = vec![0u8; sig_len];
        if sys::EVP_DigestSignFinal(md_ctx, sig.as_mut_ptr(), &mut sig_len) != 1 {
            return Err(op_err("EVP_DigestSignFinal"));
        }
        sig.truncate(sig_len);
        Ok(sig)
    }
}

/// Verify `sig` over `data` with `spki_der` (SubjectPublicKeyInfo) using
/// RSA-PSS / `hash` / MGF1(`hash`) and the requested salt length.
///
/// Returns `Ok(true)` on valid signature, `Ok(false)` on invalid.
pub fn verify_with_salt(
    spki_der: &[u8],
    hash: HashAlgo,
    data: &[u8],
    sig: &[u8],
    salt_len: i32,
) -> Result<bool, OpError> {
    unsafe {
        let pkey = parse_spki(spki_der)?;
        let _guard = PkeyGuard(pkey);

        let md = md_for(hash)?;

        let md_ctx = sys::EVP_MD_CTX_new();
        if md_ctx.is_null() {
            return Err(op_err("EVP_MD_CTX_new"));
        }
        let _ctx_guard = MdCtxGuard(md_ctx);

        let mut pctx: *mut sys::EVP_PKEY_CTX = std::ptr::null_mut();
        if sys::EVP_DigestVerifyInit(md_ctx, &mut pctx, md, std::ptr::null_mut(), pkey) != 1 {
            return Err(op_err("EVP_DigestVerifyInit"));
        }
        if sys::EVP_PKEY_CTX_set_rsa_padding(pctx, sys::RSA_PKCS1_PSS_PADDING) != 1 {
            return Err(op_err("set_rsa_padding"));
        }
        if sys::EVP_PKEY_CTX_set_rsa_pss_saltlen(pctx, salt_len) != 1 {
            return Err(op_err("set_rsa_pss_saltlen"));
        }
        if sys::EVP_PKEY_CTX_set_rsa_mgf1_md(pctx, md) != 1 {
            return Err(op_err("set_rsa_mgf1_md"));
        }

        if sys::EVP_DigestVerifyUpdate(md_ctx, data.as_ptr() as *const _, data.len()) != 1 {
            return Err(op_err("EVP_DigestVerifyUpdate"));
        }
        // Final returns 1 on valid sig, 0 on invalid, <0 on error.
        let r = sys::EVP_DigestVerifyFinal(md_ctx, sig.as_ptr(), sig.len());
        if r == 1 {
            Ok(true)
        } else if r == 0 {
            // Spec says verify just returns false on bad signature; do
            // NOT raise an exception. (RFC 3447 §8.1.2 — signature is
            // EM-recovered, padding mismatch is a "no" not an error.)
            Ok(false)
        } else {
            Err(op_err("EVP_DigestVerifyFinal"))
        }
    }
}

// -----------------------------------------------------------------------------
// RSASSA-PKCS1-v1_5 sign / verify (used for SHA-1 — aws-lc-rs only
// has SHA-1 verify, no SHA-1 sign).
// -----------------------------------------------------------------------------

pub fn pkcs1_sign(pkcs8_der: &[u8], hash: HashAlgo, data: &[u8]) -> Result<Vec<u8>, OpError> {
    unsafe {
        let pkey = parse_pkcs8(pkcs8_der)?;
        let _guard = PkeyGuard(pkey);

        let md = md_for(hash)?;
        let md_ctx = sys::EVP_MD_CTX_new();
        if md_ctx.is_null() {
            return Err(op_err("EVP_MD_CTX_new"));
        }
        let _ctx_guard = MdCtxGuard(md_ctx);

        let mut pctx: *mut sys::EVP_PKEY_CTX = std::ptr::null_mut();
        if sys::EVP_DigestSignInit(md_ctx, &mut pctx, md, std::ptr::null_mut(), pkey) != 1 {
            return Err(op_err("EVP_DigestSignInit"));
        }
        if sys::EVP_PKEY_CTX_set_rsa_padding(pctx, sys::RSA_PKCS1_PADDING) != 1 {
            return Err(op_err("set_rsa_padding(PKCS1)"));
        }

        if sys::EVP_DigestSignUpdate(md_ctx, data.as_ptr() as *const _, data.len()) != 1 {
            return Err(op_err("EVP_DigestSignUpdate"));
        }
        let mut sig_len: usize = 0;
        if sys::EVP_DigestSignFinal(md_ctx, std::ptr::null_mut(), &mut sig_len) != 1 {
            return Err(op_err("EVP_DigestSignFinal(probe)"));
        }
        let mut sig = vec![0u8; sig_len];
        if sys::EVP_DigestSignFinal(md_ctx, sig.as_mut_ptr(), &mut sig_len) != 1 {
            return Err(op_err("EVP_DigestSignFinal"));
        }
        sig.truncate(sig_len);
        Ok(sig)
    }
}

pub fn pkcs1_verify(
    spki_der: &[u8],
    hash: HashAlgo,
    data: &[u8],
    sig: &[u8],
) -> Result<bool, OpError> {
    unsafe {
        let pkey = parse_spki(spki_der)?;
        let _guard = PkeyGuard(pkey);

        let md = md_for(hash)?;
        let md_ctx = sys::EVP_MD_CTX_new();
        if md_ctx.is_null() {
            return Err(op_err("EVP_MD_CTX_new"));
        }
        let _ctx_guard = MdCtxGuard(md_ctx);

        let mut pctx: *mut sys::EVP_PKEY_CTX = std::ptr::null_mut();
        if sys::EVP_DigestVerifyInit(md_ctx, &mut pctx, md, std::ptr::null_mut(), pkey) != 1 {
            return Err(op_err("EVP_DigestVerifyInit"));
        }
        if sys::EVP_PKEY_CTX_set_rsa_padding(pctx, sys::RSA_PKCS1_PADDING) != 1 {
            return Err(op_err("set_rsa_padding(PKCS1)"));
        }

        if sys::EVP_DigestVerifyUpdate(md_ctx, data.as_ptr() as *const _, data.len()) != 1 {
            return Err(op_err("EVP_DigestVerifyUpdate"));
        }
        let r = sys::EVP_DigestVerifyFinal(md_ctx, sig.as_ptr(), sig.len());
        if r == 1 {
            Ok(true)
        } else if r == 0 {
            Ok(false)
        } else {
            Err(op_err("EVP_DigestVerifyFinal"))
        }
    }
}

// -----------------------------------------------------------------------------
// ECDSA cross-hash sign/verify (any curve × any hash) per WebCrypto §23.
// aws-lc-rs's high-level path only exposes matched-curve+hash pairs.
// EVP_DigestSign* lets us pair P-256 with SHA-512, etc. Output is the
// raw r||s wire format (W3C WebCrypto §23 step 4) — we strip the DER
// SEQUENCE that EVP emits.
// -----------------------------------------------------------------------------

pub fn ecdsa_sign(
    pkcs8_der: &[u8],
    hash: HashAlgo,
    data: &[u8],
    coord_len: usize,
) -> Result<Vec<u8>, OpError> {
    unsafe {
        let pkey = parse_pkcs8(pkcs8_der)?;
        let _guard = PkeyGuard(pkey);

        let md = md_for(hash)?;
        let md_ctx = sys::EVP_MD_CTX_new();
        if md_ctx.is_null() {
            return Err(op_err("EVP_MD_CTX_new"));
        }
        let _ctx_guard = MdCtxGuard(md_ctx);

        let mut pctx: *mut sys::EVP_PKEY_CTX = std::ptr::null_mut();
        if sys::EVP_DigestSignInit(md_ctx, &mut pctx, md, std::ptr::null_mut(), pkey) != 1 {
            return Err(op_err("ECDSA EVP_DigestSignInit"));
        }
        if sys::EVP_DigestSignUpdate(md_ctx, data.as_ptr() as *const _, data.len()) != 1 {
            return Err(op_err("ECDSA EVP_DigestSignUpdate"));
        }
        let mut sig_len: usize = 0;
        if sys::EVP_DigestSignFinal(md_ctx, std::ptr::null_mut(), &mut sig_len) != 1 {
            return Err(op_err("ECDSA EVP_DigestSignFinal(probe)"));
        }
        let mut der = vec![0u8; sig_len];
        if sys::EVP_DigestSignFinal(md_ctx, der.as_mut_ptr(), &mut sig_len) != 1 {
            return Err(op_err("ECDSA EVP_DigestSignFinal"));
        }
        der.truncate(sig_len);
        // The DER signature is `SEQUENCE { INTEGER r, INTEGER s }`.
        // Convert to fixed-length r||s for the WebCrypto wire format.
        ecdsa_der_to_p1363(&der, coord_len)
    }
}

pub fn ecdsa_verify(
    spki_der: &[u8],
    hash: HashAlgo,
    data: &[u8],
    sig: &[u8],
    coord_len: usize,
) -> Result<bool, OpError> {
    if sig.len() != 2 * coord_len {
        return Ok(false);
    }
    let der = ecdsa_p1363_to_der(sig, coord_len);
    unsafe {
        let pkey = parse_spki(spki_der)?;
        let _guard = PkeyGuard(pkey);

        let md = md_for(hash)?;
        let md_ctx = sys::EVP_MD_CTX_new();
        if md_ctx.is_null() {
            return Err(op_err("EVP_MD_CTX_new"));
        }
        let _ctx_guard = MdCtxGuard(md_ctx);

        let mut pctx: *mut sys::EVP_PKEY_CTX = std::ptr::null_mut();
        if sys::EVP_DigestVerifyInit(md_ctx, &mut pctx, md, std::ptr::null_mut(), pkey) != 1 {
            return Err(op_err("ECDSA EVP_DigestVerifyInit"));
        }
        if sys::EVP_DigestVerifyUpdate(md_ctx, data.as_ptr() as *const _, data.len()) != 1 {
            return Err(op_err("ECDSA EVP_DigestVerifyUpdate"));
        }
        let r = sys::EVP_DigestVerifyFinal(md_ctx, der.as_ptr(), der.len());
        if r == 1 {
            Ok(true)
        } else if r == 0 {
            Ok(false)
        } else {
            Err(op_err("ECDSA EVP_DigestVerifyFinal"))
        }
    }
}

/// ECDSA sign returning RAW DER (Node default wire format). Skips the
/// `ecdsa_der_to_p1363` step that the WebCrypto path applies.
pub fn ecdsa_sign_der(pkcs8_der: &[u8], hash: HashAlgo, data: &[u8]) -> Result<Vec<u8>, OpError> {
    unsafe {
        let pkey = parse_pkcs8(pkcs8_der)?;
        let _guard = PkeyGuard(pkey);
        let md = md_for(hash)?;
        let md_ctx = sys::EVP_MD_CTX_new();
        if md_ctx.is_null() {
            return Err(op_err("EVP_MD_CTX_new"));
        }
        let _ctx_guard = MdCtxGuard(md_ctx);
        let mut pctx: *mut sys::EVP_PKEY_CTX = std::ptr::null_mut();
        if sys::EVP_DigestSignInit(md_ctx, &mut pctx, md, std::ptr::null_mut(), pkey) != 1 {
            return Err(op_err("ECDSA EVP_DigestSignInit"));
        }
        if sys::EVP_DigestSignUpdate(md_ctx, data.as_ptr() as *const _, data.len()) != 1 {
            return Err(op_err("ECDSA EVP_DigestSignUpdate"));
        }
        let mut sig_len: usize = 0;
        if sys::EVP_DigestSignFinal(md_ctx, std::ptr::null_mut(), &mut sig_len) != 1 {
            return Err(op_err("ECDSA EVP_DigestSignFinal(probe)"));
        }
        let mut der = vec![0u8; sig_len];
        if sys::EVP_DigestSignFinal(md_ctx, der.as_mut_ptr(), &mut sig_len) != 1 {
            return Err(op_err("ECDSA EVP_DigestSignFinal"));
        }
        der.truncate(sig_len);
        Ok(der)
    }
}

/// ECDSA verify accepting RAW DER signatures (Node default).
pub fn ecdsa_verify_der(
    spki_der: &[u8],
    hash: HashAlgo,
    data: &[u8],
    sig: &[u8],
) -> Result<bool, OpError> {
    unsafe {
        let pkey = parse_spki(spki_der)?;
        let _guard = PkeyGuard(pkey);
        let md = md_for(hash)?;
        let md_ctx = sys::EVP_MD_CTX_new();
        if md_ctx.is_null() {
            return Err(op_err("EVP_MD_CTX_new"));
        }
        let _ctx_guard = MdCtxGuard(md_ctx);
        let mut pctx: *mut sys::EVP_PKEY_CTX = std::ptr::null_mut();
        if sys::EVP_DigestVerifyInit(md_ctx, &mut pctx, md, std::ptr::null_mut(), pkey) != 1 {
            return Err(op_err("ECDSA EVP_DigestVerifyInit"));
        }
        if sys::EVP_DigestVerifyUpdate(md_ctx, data.as_ptr() as *const _, data.len()) != 1 {
            return Err(op_err("ECDSA EVP_DigestVerifyUpdate"));
        }
        let r = sys::EVP_DigestVerifyFinal(md_ctx, sig.as_ptr(), sig.len());
        if r == 1 {
            Ok(true)
        } else if r == 0 {
            Ok(false)
        } else {
            Err(op_err("ECDSA EVP_DigestVerifyFinal"))
        }
    }
}

/// Convert DER `SEQUENCE { INTEGER r, INTEGER s }` -> r||s (each
/// `coord_len` bytes, big-endian).
fn ecdsa_der_to_p1363(der: &[u8], coord_len: usize) -> Result<Vec<u8>, OpError> {
    use crate::crypto_native::der::read_tlv_pub;
    let (top, rest) = read_tlv_pub(der)
        .ok_or_else(|| op_err("ECDSA DER: top SEQUENCE"))?;
    if !rest.is_empty() || top.tag != 0x30 {
        return Err(op_err("ECDSA DER: not a SEQUENCE"));
    }
    let body = top.value;
    let (r, body) = read_tlv_pub(body).ok_or_else(|| op_err("ECDSA DER: r INTEGER"))?;
    if r.tag != 0x02 {
        return Err(op_err("ECDSA DER: r tag"));
    }
    let (s, _) = read_tlv_pub(body).ok_or_else(|| op_err("ECDSA DER: s INTEGER"))?;
    if s.tag != 0x02 {
        return Err(op_err("ECDSA DER: s tag"));
    }
    let mut out = vec![0u8; 2 * coord_len];
    pad_int_be(r.value, &mut out[..coord_len])?;
    pad_int_be(s.value, &mut out[coord_len..])?;
    Ok(out)
}

fn pad_int_be(int_octets: &[u8], dst: &mut [u8]) -> Result<(), OpError> {
    // ASN.1 INTEGER may have a leading 0 (sign-bit guard) — strip it.
    let value = if !int_octets.is_empty() && int_octets[0] == 0 && int_octets.len() > dst.len() {
        &int_octets[1..]
    } else {
        int_octets
    };
    if value.len() > dst.len() {
        return Err(op_err("ECDSA DER: integer larger than coord"));
    }
    let off = dst.len() - value.len();
    dst[..off].fill(0);
    dst[off..].copy_from_slice(value);
    Ok(())
}

/// Convert r||s -> DER `SEQUENCE { INTEGER r, INTEGER s }`.
fn ecdsa_p1363_to_der(p1363: &[u8], coord_len: usize) -> Vec<u8> {
    let r = &p1363[..coord_len];
    let s = &p1363[coord_len..];
    let r_int = encode_asn1_integer(r);
    let s_int = encode_asn1_integer(s);
    let mut body = Vec::new();
    body.extend_from_slice(&r_int);
    body.extend_from_slice(&s_int);
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(0x30);
    push_der_len(&mut out, body.len());
    out.extend_from_slice(&body);
    out
}

fn encode_asn1_integer(be: &[u8]) -> Vec<u8> {
    // Strip leading zeros, then add a single 0 if high bit is set.
    let mut start = 0usize;
    while start < be.len() - 1 && be[start] == 0 {
        start += 1;
    }
    let trimmed = &be[start..];
    let needs_pad = !trimmed.is_empty() && (trimmed[0] & 0x80) != 0;
    let payload_len = trimmed.len() + if needs_pad { 1 } else { 0 };
    let mut out = Vec::with_capacity(payload_len + 4);
    out.push(0x02);
    push_der_len(&mut out, payload_len);
    if needs_pad {
        out.push(0);
    }
    out.extend_from_slice(trimmed);
    out
}

fn push_der_len(buf: &mut Vec<u8>, n: usize) {
    if n < 0x80 {
        buf.push(n as u8);
    } else if n < 0x100 {
        buf.push(0x81);
        buf.push(n as u8);
    } else {
        buf.push(0x82);
        buf.push((n >> 8) as u8);
        buf.push(n as u8);
    }
}

// -----------------------------------------------------------------------------
// FFI plumbing
// -----------------------------------------------------------------------------

/// Inspect an RSA SPKI blob and return its modulus length in bits.
/// Used by importKey to populate `algorithm.modulusLength` for keys
/// outside aws-lc-rs's 2048-8192 acceptance range.
pub fn rsa_spki_modulus_bits(spki: &[u8]) -> Option<u32> {
    unsafe {
        let pkey = parse_spki(spki).ok()?;
        let _guard = PkeyGuard(pkey);
        let id = sys::EVP_PKEY_id(pkey);
        if id != sys::EVP_PKEY_RSA && id != sys::EVP_PKEY_RSA_PSS {
            return None;
        }
        let bits = sys::EVP_PKEY_size(pkey) * 8;
        if bits <= 0 {
            return None;
        }
        Some(bits as u32)
    }
}

/// Same shape, for PKCS#8 RSA private keys.
pub fn rsa_pkcs8_modulus_bits(pkcs8: &[u8]) -> Option<u32> {
    unsafe {
        let pkey = parse_pkcs8(pkcs8).ok()?;
        let _guard = PkeyGuard(pkey);
        let id = sys::EVP_PKEY_id(pkey);
        if id != sys::EVP_PKEY_RSA && id != sys::EVP_PKEY_RSA_PSS {
            return None;
        }
        let bits = sys::EVP_PKEY_size(pkey) * 8;
        if bits <= 0 {
            return None;
        }
        Some(bits as u32)
    }
}

unsafe fn parse_pkcs8(pkcs8_der: &[u8]) -> Result<*mut sys::EVP_PKEY, OpError> {
    let mut p = pkcs8_der.as_ptr();
    let pkey = unsafe {
        sys::d2i_AutoPrivateKey(
            std::ptr::null_mut(),
            &mut p as *mut *const u8,
            pkcs8_der.len() as std::os::raw::c_long,
        )
    };
    if pkey.is_null() {
        return Err(OpError::dom(
            "DataError",
            "RSA-PSS: PKCS#8 private-key parse failed",
        ));
    }
    Ok(pkey)
}

unsafe fn parse_spki(spki_der: &[u8]) -> Result<*mut sys::EVP_PKEY, OpError> {
    let mut p = spki_der.as_ptr();
    let pkey = unsafe {
        sys::d2i_PUBKEY(
            std::ptr::null_mut(),
            &mut p as *mut *const u8,
            spki_der.len() as std::os::raw::c_long,
        )
    };
    if pkey.is_null() {
        return Err(OpError::dom(
            "DataError",
            "RSA-PSS: SPKI public-key parse failed",
        ));
    }
    Ok(pkey)
}

fn md_for(hash: HashAlgo) -> Result<*const sys::EVP_MD, OpError> {
    let md = match hash {
        HashAlgo::Sha1 => unsafe { sys::EVP_sha1() },
        HashAlgo::Sha256 => unsafe { sys::EVP_sha256() },
        HashAlgo::Sha384 => unsafe { sys::EVP_sha384() },
        HashAlgo::Sha512 => unsafe { sys::EVP_sha512() },
    };
    if md.is_null() {
        return Err(op_err("EVP_sha*"));
    }
    Ok(md)
}

fn op_err(stage: &str) -> OpError {
    OpError::dom(
        "OperationError",
        format!("RSA-PSS variable-salt: {stage} failed"),
    )
}

struct PkeyGuard(*mut sys::EVP_PKEY);
impl Drop for PkeyGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { sys::EVP_PKEY_free(self.0) };
        }
    }
}

struct MdCtxGuard(*mut sys::EVP_MD_CTX);
impl Drop for MdCtxGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { sys::EVP_MD_CTX_free(self.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::encoding::AsDer;

    fn gen_rsa_2048_pkcs8_and_spki() -> (Vec<u8>, Vec<u8>) {
        let priv_key =
            aws_lc_rs::rsa::PrivateDecryptingKey::generate(aws_lc_rs::rsa::KeySize::Rsa2048)
                .unwrap();
        let pkcs8: aws_lc_rs::encoding::Pkcs8V1Der<'static> = AsDer::as_der(&priv_key).unwrap();
        let pub_part = priv_key.public_key();
        let spki: aws_lc_rs::encoding::PublicKeyX509Der<'static> = AsDer::as_der(&pub_part).unwrap();
        (pkcs8.as_ref().to_vec(), spki.as_ref().to_vec())
    }

    #[test]
    fn round_trip_salt_zero_deterministic() {
        let (pkcs8, spki) = gen_rsa_2048_pkcs8_and_spki();
        let data = b"deterministic-pss";

        // Salt length 0 -> deterministic. Two signatures with identical
        // inputs should be byte-equal.
        let sig1 = sign_with_salt(&pkcs8, HashAlgo::Sha256, data, 0).unwrap();
        let sig2 = sign_with_salt(&pkcs8, HashAlgo::Sha256, data, 0).unwrap();
        assert_eq!(sig1, sig2, "salt=0 sign must be deterministic");

        // Verify accepts both.
        assert!(verify_with_salt(&spki, HashAlgo::Sha256, data, &sig1, 0).unwrap());
        assert!(verify_with_salt(&spki, HashAlgo::Sha256, data, &sig2, 0).unwrap());
    }

    #[test]
    fn round_trip_salt_default_digest_length() {
        let (pkcs8, spki) = gen_rsa_2048_pkcs8_and_spki();
        let data = b"digest-len-pss";
        let n = HashAlgo::Sha256.digest_len() as i32; // 32

        let sig = sign_with_salt(&pkcs8, HashAlgo::Sha256, data, n).unwrap();
        assert!(verify_with_salt(&spki, HashAlgo::Sha256, data, &sig, n).unwrap());
    }

    #[test]
    fn round_trip_salt_large() {
        // 128-byte salt for a 2048-bit RSA / SHA-256 — within the
        // (emLen − hLen − 2) cap. RFC 3447 §9.1.1.
        let (pkcs8, spki) = gen_rsa_2048_pkcs8_and_spki();
        let data = b"large-salt-pss";
        let n = 128i32;

        let sig = sign_with_salt(&pkcs8, HashAlgo::Sha256, data, n).unwrap();
        assert!(verify_with_salt(&spki, HashAlgo::Sha256, data, &sig, n).unwrap());
    }

    #[test]
    fn verify_rejects_wrong_data() {
        let (pkcs8, spki) = gen_rsa_2048_pkcs8_and_spki();
        let sig = sign_with_salt(&pkcs8, HashAlgo::Sha256, b"abc", 32).unwrap();
        // Different data — must verify=false (not error).
        let ok = verify_with_salt(&spki, HashAlgo::Sha256, b"abd", &sig, 32).unwrap();
        assert!(!ok);
    }

    #[test]
    fn cross_salt_length_does_not_verify() {
        // Sign with salt=0, verify with salt=32. EVP_PKEY_CTX has the
        // verify-side enforcement of saltLength match (or auto-detect
        // when -2 is passed; we explicitly enforce). Should fail.
        let (pkcs8, spki) = gen_rsa_2048_pkcs8_and_spki();
        let sig = sign_with_salt(&pkcs8, HashAlgo::Sha256, b"x", 0).unwrap();
        let ok = verify_with_salt(&spki, HashAlgo::Sha256, b"x", &sig, 32).unwrap();
        assert!(!ok, "verifying with mismatched saltLength should reject");
    }
}
