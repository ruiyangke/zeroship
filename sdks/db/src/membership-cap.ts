/**
 * Largest `$in` membership list any SDK path may send in one native call.
 *
 * MUST NOT exceed `MAX_MEMBERSHIP_LIST_LEN` in
 * `crates/zeroship-data-query-builder/src/compile.rs`, which **rejects** a longer list
 * outright rather than clamping it. The two are separate constants in separate
 * languages with no compile-time link, so raising one without the other turns
 * every over-cap call into a hard error.
 *
 * It lives in its own module because there is more than one emitter, and the
 * first fix for this defect chunked only one of them: the relation loader was
 * capped while `IdLoader`'s batched `get()` path kept sending an unbounded
 * list. A shared constant is the cheap half of not repeating that; the
 * regression tests that assert each emitter's batches stay within it are the
 * half that actually holds.
 */
export const MAX_ID_BATCH = 100;
