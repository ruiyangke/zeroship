// op.* migration fixture — the §2.5 cross-implementation scalar footguns carried
// through the typed-bind domain, authored via the fluent table() surface. Each is
// asserted to round-trip + checksum identically on both the JS and Rust sides:
//   - a large integer BEYOND the JS safe-integer range, carried via decimal()
//     (a bare JS number >= 2^53 is rejected at load - the recorder must emit the
//     decimal carrier);
//   - a fractional value carried via decimal() (a JS float is rejected at load);
//   - a unicode + escape-bearing string literal;
//   - a boolean and a null.
import { table, decimal } from "zero-migrate";

export const name = "edge_scalars";

export function up() {
  table("edge").insert({
    rows: [
      {
        big: decimal("9007199254740993"), // 2^53 + 1, exact via decimal-string
        frac: decimal("1.5"), // fractional via decimal-string (no JS float in IR)
        uni: "héllo\t\"world\" 𝟙", // non-ASCII + escape + supplementary-plane code point
        flag: true,
        missing: null,
      },
    ],
  });
}
