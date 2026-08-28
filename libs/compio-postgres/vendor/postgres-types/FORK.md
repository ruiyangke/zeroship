# postgres-types fork

This directory is `postgres-types` 0.2.14 from
`rust-postgres/rust-postgres`, retained under its upstream MIT OR Apache-2.0
license. It is source-local because Rust's orphan rules do not let
`compio-postgres` replace `FromSql`/`ToSql` implementations whose trait and
target type are both upstream.

The intentional delta is limited to binary type correctness:

- classify built-in `_record` as `Array(RECORD)` so the generic array decoder
  can dispatch legal `record[]` output;
- reject PostgreSQL `time '24:00'` in direct chrono/time targets that cannot
  represent it, and expose `TimeOfDay::EndOfDay` for a lossless round trip;
- checked-convert `SystemTime` and refuse timestamp infinity in the bare type,
  leaving `Timestamp<SystemTime>` as the lossless infinity carrier.

Every item has a live regression in `libs/compio-postgres/tests/suite`. When
updating the fork, replace it from the new upstream release, reapply only deltas
still absent there, and run those tests before changing this note.
