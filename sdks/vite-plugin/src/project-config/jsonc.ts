/**
 * A minimal JSONC reader: comments and trailing commas, nothing else.
 *
 * The byte-for-byte peer of `crates/cli/src/project_config/jsonc.rs`. The two
 * are written twice on purpose - the alternative is a Node subprocess in the
 * Rust CLI, which is exactly the wall (`crates/cli/src/migrate.rs:9-14`) that
 * produced the drift this file exists to remove.
 *
 * WHAT KEEPS THEM HONEST is not this comment: `tests/project_config_gate.sh`
 * parses one fixture with BOTH readers and byte-compares the resolved dumps,
 * and the fixture carries the cases where a naive stripper diverges (a `//`
 * inside a URL, an escaped quote before a `/*`, a comma inside a string, a
 * trailing comma, multi-byte content).
 *
 * Comments are replaced with SPACES rather than removed so every offset in the
 * stripped text is the same offset in the file. The Rust side needs that for
 * the writeback splice; keeping the algorithms identical is worth more than the
 * few bytes.
 */

/** Replace comments and trailing commas with spaces, preserving offsets. */
export function stripJsonc(text: string): string {
  const src = Buffer.from(text, "utf8");
  const out = Buffer.from(src);
  let i = 0;
  let inString = false;

  while (i < src.length) {
    const b = src[i];
    if (inString) {
      if (b === 0x5c /* \ */) {
        i += 2;
        continue;
      }
      if (b === 0x22 /* " */) inString = false;
      i += 1;
      continue;
    }
    if (b === 0x22) {
      inString = true;
      i += 1;
      continue;
    }
    if (b === 0x2f /* / */ && src[i + 1] === 0x2f) {
      while (i < src.length && src[i] !== 0x0a) out[i++] = 0x20;
      continue;
    }
    if (b === 0x2f && src[i + 1] === 0x2a /* * */) {
      out[i] = 0x20;
      out[i + 1] = 0x20;
      i += 2;
      while (i < src.length) {
        if (src[i] === 0x2a && src[i + 1] === 0x2f) {
          out[i] = 0x20;
          out[i + 1] = 0x20;
          i += 2;
          break;
        }
        if (src[i] !== 0x0a) out[i] = 0x20;
        i += 1;
      }
      continue;
    }
    i += 1;
  }

  blankTrailingCommas(out);
  return out.toString("utf8");
}

const WS = new Set([0x20, 0x09, 0x0a, 0x0d]);

function blankTrailingCommas(buf: Buffer): void {
  let inString = false;
  let i = 0;
  while (i < buf.length) {
    const b = buf[i];
    if (inString) {
      if (b === 0x5c) {
        i += 2;
        continue;
      }
      if (b === 0x22) inString = false;
      i += 1;
      continue;
    }
    if (b === 0x22) {
      inString = true;
      i += 1;
      continue;
    }
    if (b === 0x2c /* , */) {
      let j = i + 1;
      while (j < buf.length && WS.has(buf[j])) j += 1;
      if (j < buf.length && (buf[j] === 0x7d /* } */ || buf[j] === 0x5d /* ] */)) {
        buf[i] = 0x20;
      }
    }
    i += 1;
  }
}
