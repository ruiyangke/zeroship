//! `Accept-Encoding` negotiation — pick a pre-compressed variant
//! (br/gzip) when the client accepts one, else fall back to identity.
//!
//! The static-serve path consults this to choose which blob to serve;
//! ETag, Content-Length, Content-Range, and the conditional-GET match
//! all reflect the chosen variant.

/// Result of `Accept-Encoding` negotiation against an asset's
/// `variants` map. Three shapes:
///
/// * `(hash, size, None)`         — identity body. The caller emits no
///                                  `Content-Encoding` and no `Vary`.
/// * `(hash, size, Some(enc))`    — variant body. Caller sets
///                                  `Content-Encoding: <enc>` and
///                                  `Vary: Accept-Encoding`.
#[derive(Debug, Clone)]
pub(super) struct ChosenVariant {
    pub(super) hash: String,
    pub(super) size: u64,
    /// `None` → identity. `Some(enc)` → compressed variant.
    pub(super) encoding: Option<String>,
}

/// Pick the best encoding for the request's `Accept-Encoding` against
/// the asset's available variants. Falls back to identity when no
/// variant is offered or none of the offered encodings are accepted.
///
/// q-value handling: any encoding listed with `q=0` is treated as
/// rejected (matches RFC 7231 §5.3.4); otherwise we walk the request's
/// listed encodings in order and return the first one we have a
/// variant for. Listed-encodings order is the client's preference
/// signal — Chrome / Firefox put `br` before `gzip`, which is what
/// we want, so a simple in-order walk does the right thing without a
/// full q-value sort.
///
/// Special tokens:
/// * `*` (any) — matches any variant we have. Picked only when no
///   explicit variant was listed first.
/// * `identity` — explicitly request identity; we honour it.
pub(super) fn pick_variant(
    hit: &crate::dispatch::StaticHit,
    accept_encoding: Option<&str>,
) -> ChosenVariant {
    let identity = ChosenVariant {
        hash: hit.hash.clone(),
        size: hit.size,
        encoding: None,
    };

    let header = match accept_encoding {
        Some(h) => h,
        None => return identity,
    };
    if hit.variants.is_empty() {
        return identity;
    }

    // Parse `Accept-Encoding` into (token, accepted?) pairs, preserving
    // request order (which captures client preference for the common
    // browser case).
    //
    // Examples:
    //   "br, gzip"            → [("br", true), ("gzip", true)]
    //   "gzip;q=0.5, br;q=1"  → [("gzip", true), ("br", true)]  (q values >0 → accept)
    //   "identity;q=0, *"     → [("identity", false), ("*", true)]
    let mut accepted: Vec<&str> = Vec::with_capacity(4);
    let mut wildcard = false;
    let mut wildcard_rejected = false;
    let mut identity_rejected = false;
    for raw in header.split(',') {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        let (token, q_zero) = parse_accept_encoding_part(part);
        if token == "*" {
            if q_zero {
                wildcard_rejected = true;
            } else {
                wildcard = true;
            }
            continue;
        }
        if token.eq_ignore_ascii_case("identity") {
            if q_zero {
                identity_rejected = true;
            }
            // Identity isn't a variant — we don't push it onto the
            // accepted list; we just track its rejection.
            continue;
        }
        if !q_zero {
            accepted.push(token);
        }
    }

    // Walk the client's preferred order. First variant we have wins.
    for token in &accepted {
        if let Some(variant) = hit.variants.get(*token) {
            return ChosenVariant {
                hash: variant.hash.clone(),
                size: variant.size,
                encoding: Some((*token).to_string()),
            };
        }
    }

    // Wildcard: pick any variant we have. Prefer `br` then `gzip` for
    // determinism (browsers don't typically send wildcard, but proxies
    // and CLIs do).
    if wildcard && !wildcard_rejected {
        for enc in &["br", "gzip"] {
            if let Some(variant) = hit.variants.get(*enc) {
                return ChosenVariant {
                    hash: variant.hash.clone(),
                    size: variant.size,
                    encoding: Some((*enc).to_string()),
                };
            }
        }
    }

    // Identity rejected explicitly AND no variant matched? RFC 7231
    // says we MAY return 406 here; in practice 99% of Accept-Encoding
    // headers list `identity;q=0` only as a hint, not a hard demand,
    // and serving identity is universally accepted by the actual
    // client even when the header would technically forbid it. Match
    // browsers' permissive behaviour.
    let _ = identity_rejected;
    identity
}

/// Parse one `Accept-Encoding` token segment and return its name and
/// whether it carries `q=0`. Anything else (`q=0.5`, no q at all, …)
/// → accepted.
pub(super) fn parse_accept_encoding_part(part: &str) -> (&str, bool) {
    if let Some((name, params)) = part.split_once(';') {
        let name = name.trim();
        for p in params.split(';') {
            let p = p.trim();
            if let Some(qval) = p.strip_prefix("q=").or_else(|| p.strip_prefix("Q=")) {
                if let Ok(q) = qval.parse::<f32>() {
                    if q <= 0.0 {
                        return (name, true);
                    }
                }
            }
        }
        (name, false)
    } else {
        (part.trim(), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zeroship_core::types::AssetVariant;

    /// 64-char hex hash for tests; the disk cache shards on the first
    /// two chars, so a real-shaped hash exercises that path even though
    /// the variants tests don't go to disk.
    fn hex_hash(byte: u8) -> String {
        let mut s = format!("{byte:02x}");
        s.push_str(&"e".repeat(62));
        s
    }

    /// Build a minimal [`StaticHit`](crate::dispatch::StaticHit) carrying
    /// the given identity hash/size and `variants` map. The default
    /// `cache`/`content_type` shape doesn't matter — `pick_variant`
    /// only reads `hash`, `size`, and `variants`.
    fn static_hit_with_variants(
        identity_byte: u8,
        identity_size: u64,
        variants: HashMap<String, AssetVariant>,
    ) -> crate::dispatch::StaticHit {
        crate::dispatch::StaticHit {
            path: "/asset".into(),
            hash: hex_hash(identity_byte),
            content_type: "application/octet-stream".into(),
            size: identity_size,
            cache: zeroship_core::types::CacheCtl {
                max_age: 60,
                swr_window: None,
                immutable: false,
                background_refresh: false,
                stale_on_error: false,
            },
            status: None,
            mutable: false,
            variants,
        }
    }

    #[test]
    fn pick_variant_no_header_returns_identity() {
        let hit = static_hit_with_variants(
            0x01,
            1024,
            HashMap::from([(
                "br".into(),
                AssetVariant { hash: hex_hash(0xBB), size: 256 },
            )]),
        );
        let chosen = pick_variant(&hit, None);
        assert!(chosen.encoding.is_none(), "no Accept-Encoding → identity");
        assert_eq!(chosen.hash, hit.hash);
        assert_eq!(chosen.size, hit.size);
    }

    #[test]
    fn pick_variant_no_variants_map_returns_identity() {
        let hit = static_hit_with_variants(0x02, 1024, HashMap::new());
        let chosen = pick_variant(&hit, Some("br, gzip"));
        assert!(chosen.encoding.is_none());
        assert_eq!(chosen.hash, hit.hash);
    }

    #[test]
    fn pick_variant_brotli_preferred_over_gzip() {
        let br_hash = hex_hash(0xBB);
        let gz_hash = hex_hash(0x6F);
        let hit = static_hit_with_variants(
            0x03,
            10000,
            HashMap::from([
                ("br".into(), AssetVariant { hash: br_hash.clone(), size: 1500 }),
                ("gzip".into(), AssetVariant { hash: gz_hash.clone(), size: 2500 }),
            ]),
        );
        // Client lists br first → server picks br.
        let chosen = pick_variant(&hit, Some("br, gzip"));
        assert_eq!(chosen.encoding.as_deref(), Some("br"));
        assert_eq!(chosen.hash, br_hash);
        assert_eq!(chosen.size, 1500);
    }

    #[test]
    fn pick_variant_gzip_when_only_gzip_accepted() {
        let gz_hash = hex_hash(0x6F);
        let hit = static_hit_with_variants(
            0x04,
            10000,
            HashMap::from([
                ("br".into(), AssetVariant { hash: hex_hash(0xBB), size: 1500 }),
                ("gzip".into(), AssetVariant { hash: gz_hash.clone(), size: 2500 }),
            ]),
        );
        let chosen = pick_variant(&hit, Some("gzip"));
        assert_eq!(chosen.encoding.as_deref(), Some("gzip"));
        assert_eq!(chosen.hash, gz_hash);
    }

    #[test]
    fn pick_variant_identity_explicit() {
        let hit = static_hit_with_variants(
            0x05,
            1024,
            HashMap::from([(
                "br".into(),
                AssetVariant { hash: hex_hash(0xBB), size: 256 },
            )]),
        );
        let chosen = pick_variant(&hit, Some("identity"));
        assert!(chosen.encoding.is_none(), "identity-only → identity served");
    }

    #[test]
    fn pick_variant_q_zero_rejects() {
        // gzip;q=0 means "do NOT send gzip". Server must fall back to
        // brotli (still accepted) or identity (always available).
        let hit = static_hit_with_variants(
            0x06,
            1024,
            HashMap::from([
                ("br".into(), AssetVariant { hash: hex_hash(0xBB), size: 256 }),
                ("gzip".into(), AssetVariant { hash: hex_hash(0x6F), size: 384 }),
            ]),
        );
        let chosen = pick_variant(&hit, Some("br, gzip;q=0"));
        assert_eq!(chosen.encoding.as_deref(), Some("br"));
    }

    #[test]
    fn pick_variant_unknown_encoding_falls_back_to_identity() {
        let hit = static_hit_with_variants(
            0x07,
            1024,
            HashMap::from([(
                "br".into(),
                AssetVariant { hash: hex_hash(0xBB), size: 256 },
            )]),
        );
        // Client only accepts `lz4` which we don't have a variant for
        // → identity falls through.
        let chosen = pick_variant(&hit, Some("lz4"));
        assert!(chosen.encoding.is_none());
    }
}
