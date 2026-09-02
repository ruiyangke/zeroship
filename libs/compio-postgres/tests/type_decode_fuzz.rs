//! Randomised totality checks for the value decoders.
//!
//! Every byte handed to `FromSql::from_sql` is chosen by the server, so the
//! contract is TOTALITY: return `Ok` or `Err` for any input, never panic. The
//! decoders parse structure (lengths, counts, dimensions, tags), and the
//! vendored `postgres-types` tree they live in is excluded from this crate's
//! coverage runs, so nothing else sweeps them.
//!
//! Lengths are biased toward the EXACT sizes the fixed-width decoders demand.
//! Uniformly random lengths are rejected by the leading length check and never
//! reach the decoder body - the same vacuity that made the first draft of the
//! frame fuzzer prove nothing.

use compio_postgres::types::{FromSql, Type};

struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        ((self.next_u64() >> 33) as usize) % n
    }
    fn byte(&mut self) -> u8 {
        (self.next_u64() & 0xff) as u8
    }
}

/// Sizes the fixed-width decoders accept, plus shapes around them.
const LENGTHS: &[usize] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 20, 24, 32];

fn body(rng: &mut Rng) -> Vec<u8> {
    let len = LENGTHS[rng.below(LENGTHS.len())];
    (0..len).map(|_| rng.byte()).collect()
}

/// A PostgreSQL array body: ndim, flags, element OID, then per-dimension
/// length and lower bound, then each element as a length-prefixed value.
///
/// Uniformly random bytes NEVER get past `array_from_sql`: measured with a
/// probe panic in the decoder body, 62000 random inputs reached it zero times.
/// The fields are therefore chosen from plausible values, with room to be
/// wrong, so the body runs and its count arithmetic is what gets swept.
fn structured_array(rng: &mut Rng) -> Vec<u8> {
    const NDIMS: &[i32] = &[0, 1, 1, 1, 2, 3, -1, 7, i32::MAX];
    const LENS: &[i32] = &[0, 1, 2, 3, -1, i32::MAX];
    const BOUNDS: &[i32] = &[1, 0, -1, i32::MIN];
    const ELEM: &[i32] = &[-1, 0, 1, 4, 8, i32::MAX];

    let ndim = NDIMS[rng.below(NDIMS.len())];
    let mut out = Vec::new();
    out.extend_from_slice(&ndim.to_be_bytes());
    out.extend_from_slice(&(rng.below(3) as i32).to_be_bytes());
    out.extend_from_slice(&23u32.to_be_bytes());

    let dims = ndim.clamp(0, 4) as usize;
    let mut elements = 1usize;
    for _ in 0..dims {
        let len = LENS[rng.below(LENS.len())];
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&BOUNDS[rng.below(BOUNDS.len())].to_be_bytes());
        elements = elements.saturating_mul(len.clamp(0, 4) as usize);
    }
    for _ in 0..elements.min(8) {
        let len = ELEM[rng.below(ELEM.len())];
        out.extend_from_slice(&len.to_be_bytes());
        for _ in 0..len.clamp(0, 8) {
            out.push(rng.byte());
        }
    }
    out
}

/// Decode one (Rust type, PostgreSQL type) pair, counting outcomes. A panic
/// here fails the test, which is the whole point.
macro_rules! sweep {
    ($rng:expr, $ok:expr, $err:expr, $( $t:ty => $ty:expr ),* $(,)?) => {
        $(
            for _ in 0..2_000 {
                let raw = body($rng);
                match <$t as FromSql>::from_sql(&$ty, &raw) {
                    Ok(_) => $ok += 1,
                    Err(_) => $err += 1,
                }
            }
        )*
    };
}

#[test]
fn hostile_value_bytes_are_refused_rather_than_panicking() {
    let mut rng = Rng(0x7A11_C0DE_5EED_0001);
    let (mut ok, mut err) = (0u32, 0u32);
    let mut array_ok = 0u32;

    sweep!(
        &mut rng, ok, err,
        bool => Type::BOOL,
        i8 => Type::CHAR,
        i16 => Type::INT2,
        i32 => Type::INT4,
        i64 => Type::INT8,
        u32 => Type::OID,
        f32 => Type::FLOAT4,
        f64 => Type::FLOAT8,
        String => Type::TEXT,
        String => Type::VARCHAR,
        Vec<u8> => Type::BYTEA,
        std::net::IpAddr => Type::INET,
    );

    #[cfg(feature = "array-impls")]
    for _ in 0..8_000 {
        let raw = structured_array(&mut rng);
        match <Vec<i32> as FromSql>::from_sql(&Type::INT4_ARRAY, &raw) {
            Ok(_) => array_ok += 1,
            Err(_) => err += 1,
        }
    }
    #[cfg(feature = "with-bit-vec-0_9")]
    sweep!(&mut rng, ok, err, bit_vec::BitVec => Type::VARBIT, bit_vec::BitVec => Type::BIT);
    #[cfg(feature = "with-serde_json-1")]
    sweep!(&mut rng, ok, err, serde_json::Value => Type::JSONB, serde_json::Value => Type::JSON);
    #[cfg(feature = "with-uuid-1")]
    sweep!(&mut rng, ok, err, uuid::Uuid => Type::UUID);
    #[cfg(feature = "with-eui48-1")]
    sweep!(&mut rng, ok, err, eui48::MacAddress => Type::MACADDR);
    #[cfg(feature = "with-geo-types-0_7")]
    sweep!(&mut rng, ok, err, geo_types::Point<f64> => Type::POINT, geo_types::Rect<f64> => Type::BOX);
    #[cfg(feature = "with-cidr-0_3")]
    sweep!(&mut rng, ok, err, cidr::IpInet => Type::INET, cidr::IpCidr => Type::CIDR);
    #[cfg(feature = "with-chrono-0_4")]
    sweep!(&mut rng, ok, err, chrono::NaiveDateTime => Type::TIMESTAMP, chrono::NaiveDate => Type::DATE, chrono::NaiveTime => Type::TIME);
    #[cfg(feature = "with-time-0_3")]
    sweep!(&mut rng, ok, err, time::OffsetDateTime => Type::TIMESTAMPTZ, time::Date => Type::DATE);
    #[cfg(feature = "with-jiff-0_2")]
    sweep!(&mut rng, ok, err, jiff::Timestamp => Type::TIMESTAMPTZ, jiff::civil::Date => Type::DATE);

    eprintln!("value-decoder fuzz: ok={ok} array_ok={array_ok} err={err}");
    assert!(
        ok > 200,
        "the generator never produced a decodable value, so it swept nothing: ok={ok}"
    );
    assert!(
        err > 200,
        "the generator never exercised a refusal: err={err}"
    );
    #[cfg(feature = "array-impls")]
    assert!(
        array_ok > 100,
        "no generated array decoded, so the array body was never swept: array_ok={array_ok}"
    );
}
