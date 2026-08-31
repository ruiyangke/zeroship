# The 50 `as` casts in compio-postgres, audited

Measured 2026-08-30 at `a17cdbfdd`, production code only (every
`#[cfg(...test...)]` item excised by brace matching).

## Counts

    production `as` casts            50
    with a narrow TARGET type        22
    of those, with a justifying comment within 5 lines   2

**Caveat on my own instrument:** the 22 is bucketed by TARGET type, not by
whether the conversion actually loses range. `tls_rustls.rs:657` is
`u16::from_be_bytes(..) as u32` - a widening - and `replication.rs:1908` splits
a `u64` LSN into two `u32` halves deliberately. Read 22 as "worth looking at",
not as "22 truncation bugs".

## What the sample shows

Benign by construction, no action:

    client.rs:1201,1202,1452,1464   `Self::In as u8` - C-like enum discriminant
    prepare.rs:534,537,548          `b'e' as i8` - byte literal constant
    query.rs:780                    encode_format(..) as i16 - small enum
    cancel_query_raw.rs:188         packet_len as u32 - a cancel packet is 16 bytes
    replication.rs:1908             u64 LSN -> two u32 halves, which is exactly
                                    how PostgreSQL formats `X/X`

Correct AND documented - the good pattern:

    codec.rs:417,430,458   `header.len() as u32`, carrying its own argument:
      "below 4, so the value is in `4..=i32::MAX` and this cast is exact."
    and `validate_length(length: u32)` then widens to u64 before comparing
    (`1u64 + u64::from(length)`), so the addition cannot overflow either.

Worth a second look, not yet judged:

    replication.rs:2313    `read_u32(buf)? as i32` - a reinterpret, negative for
                           values above i32::MAX. Whether that is reachable
                           depends on the field; unverified here.
    tls_rustls.rs:621-626  DER length encoding, `contents.len() as u8` after a
                           width branch. Standard DER, but the guard and the
                           cast are several lines apart.

## The actual finding

The gap is DOCUMENTATION, not a pile of latent truncation bugs. **Two of
twenty-two** narrow-target casts carry a justification within five lines. The
`codec.rs` sites show what the rest should look like: state the range invariant
that makes the cast exact, at the cast.

`clippy::pedantic` warns `cast_possible_truncation` and pedantic is warn, not
deny, so none of these are enforced. Raising that lint to deny would require
either a justification or an explicit `try_into()` at each site - a reasonable
end state, but it is 22 edits and should be its own change, not a drive-by.
