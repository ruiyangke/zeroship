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

## The 30 production `expect`s: a strength, not a gap

I listed these alongside the casts as an unaudited risk. Audited 2026-08-30,
they are the opposite: **25 of 30 state the invariant that makes the call
infallible**, which is exactly what the standard library recommends an `expect`
message to do - say why the value is expected, not what went wrong.

    fill(5) guarantees 5 bytes are buffered
    a guard holds its name until it is disarmed exactly once
    Endpoint::addresses rejects an empty address list
    async header implies full message is buffered
    the first ErrorResponse was parsed above
    a live TLS lease owns a session
    the guard proved the path is present

The five that do not follow the pattern are terse rather than wrong -
`"checked above"` (three sites in `connection.rs`) points at a check without
naming it, and `"read obligation overflow"` names the failure instead of the
invariant. Worth tightening if that code is touched; not worth a sweep.

## Scanning production-only code: use one tool, with a self-check

Four separate attempts at "count X in production code" were wrong four
different ways this session:

    truncate at the first `^#[cfg(test)]`   read 499 of 3651 lines of client.rs
    truncate at the first `#[cfg(test)]` anywhere   cut at a COMMENT mentioning it
    strip comments with re.sub, then use offsets   every line number shifted early
    `#\[cfg\([^)]*\btest\b[^)]*\)\]`        cannot match `#[cfg(all(test, ..))]`
                                            because `[^)]*` stops at the first `)`

The last one silently pulled 48 test-only `expect`s from `connect_socket.rs`
into a "production" count, turning 30 into 78.

What works: blank `//` comments IN PLACE (preserving length so offsets stay
valid), match `#\[cfg\([^\]]*\btest\b[^\]]*\]` so nested `all(..)`/`any(..)`
still matches, brace-match each annotated item, and test membership by span.

**And give it a self-check with a known answer.** `connect_socket.rs` is one
large `#[cfg(all(test, target_os = "linux"))]` block, so any production scan
that attributes even one line to it is broken. That single assertion catches
every failure above.
