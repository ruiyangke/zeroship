//! `node:buffer` — Buffer class extending Uint8Array, the static
//! factories (alloc / from / byteLength / compare / concat / isBuffer),
//! the prototype methods (toString / write / fill / copy / equals /
//! indexOf / read*/write* numerics / toJSON), and the
//! `globalThis.Buffer` install. Migrated from unenv polyfill in #204.

mod common;
use common::{dispatch, m};

// ---------------------------------------------------------------------------
// Static factories — Buffer.from / Buffer.alloc / Buffer.of
// ---------------------------------------------------------------------------

#[test]
fn from_string_default_utf8() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            const b = Buffer.from("hello");
            return { len: b.length, isU8: b instanceof Uint8Array };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""len":5"#), "got: {}", r.json);
    assert!(r.json.contains(r#""isU8":true"#), "got: {}", r.json);
}

#[test]
fn from_string_utf8_round_trip() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            return Buffer.from("hello", "utf-8").toString("utf-8");
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("hello"), "got: {}", r.json);
}

#[test]
fn from_array_decodes_to_string() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            return Buffer.from([0x68, 0x69]).toString();
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("hi"), "got: {}", r.json);
}

#[test]
fn alloc_with_fill_byte() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            const b = Buffer.alloc(4, 0xff);
            return Array.from(b);
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("[255,255,255,255]"), "got: {}", r.json);
}

#[test]
fn byte_length_utf8_multibyte() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            return { n: Buffer.byteLength("héllo", "utf-8") };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    // é is two UTF-8 bytes, so 5 chars → 6 bytes
    assert!(r.json.contains(r#""n":6"#), "got: {}", r.json);
}

#[test]
fn compare_orders_lexicographically() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            return { c: Buffer.compare(Buffer.from("a"), Buffer.from("b")) };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""c":-1"#), "got: {}", r.json);
}

#[test]
fn concat_joins_buffers() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            const b = Buffer.concat([Buffer.from("ab"), Buffer.from("cd")]);
            return b.toString();
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("abcd"), "got: {}", r.json);
}

#[test]
fn is_buffer_distinguishes_uint8array() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            return {
                ua: Buffer.isBuffer(new Uint8Array(4)),
                buf: Buffer.isBuffer(Buffer.alloc(4)),
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""ua":false"#), "got: {}", r.json);
    assert!(r.json.contains(r#""buf":true"#), "got: {}", r.json);
}

#[test]
fn equals_byte_for_byte() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            const a = Buffer.from("hello");
            const b = Buffer.from("hello");
            const c = Buffer.from("world");
            return { eq: a.equals(b), neq: a.equals(c) };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""eq":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""neq":false"#), "got: {}", r.json);
}

#[test]
fn index_of_finds_substring() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            return { i: Buffer.from("hello").indexOf("ll") };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""i":2"#), "got: {}", r.json);
}

#[test]
fn hex_decoding() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            return Buffer.from("48656c6c6f", "hex").toString();
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("Hello"), "got: {}", r.json);
}

#[test]
fn base64_decoding() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            return Buffer.from("aGVsbG8=", "base64").toString();
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("hello"), "got: {}", r.json);
}

#[test]
fn write_read_uint32_le_round_trip() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            const b = Buffer.alloc(4);
            b.writeUInt32LE(0x12345678, 0);
            return { v: b.readUInt32LE(0) };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""v":305419896"#), "got: {}", r.json);
}

#[test]
fn buffer_is_uint8array_subclass() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            return { sub: Buffer.from("hello") instanceof Uint8Array };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""sub":true"#), "got: {}", r.json);
}

#[test]
fn global_buffer_matches_imported() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            return { same: globalThis.Buffer === Buffer };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""same":true"#), "got: {}", r.json);
}

#[test]
fn to_json_emits_node_shape() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            return Buffer.from("hello").toJSON();
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""type":"Buffer""#), "got: {}", r.json);
    assert!(
        r.json.contains(r#""data":[104,101,108,108,111]"#),
        "got: {}",
        r.json
    );
}

// ---------------------------------------------------------------------------
// Additional sanity — encoding aliases, subarray, fill, default import
// ---------------------------------------------------------------------------

#[test]
fn default_import_returns_namespace_object() {
    let r = dispatch(
        m(r#"
        import buffer from "node:buffer";
        export function test() {
            return {
                hasBuffer: typeof buffer.Buffer === "function",
                hasMaxLen: typeof buffer.kMaxLength === "number",
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""hasBuffer":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasMaxLen":true"#), "got: {}", r.json);
}

#[test]
fn subarray_returns_buffer_not_uint8array() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            const slice = Buffer.from("hello").subarray(1, 4);
            return { isBuf: Buffer.isBuffer(slice), s: slice.toString() };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""isBuf":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""s":"ell""#), "got: {}", r.json);
}

#[test]
fn is_encoding_recognises_aliases() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            return {
                utf: Buffer.isEncoding("UTF-8"),
                hex: Buffer.isEncoding("hex"),
                bogus: Buffer.isEncoding("not-real"),
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""utf":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hex":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""bogus":false"#), "got: {}", r.json);
}

#[test]
fn variable_byte_uint_le_round_trip() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            const b = Buffer.alloc(6);
            b.writeUIntLE(0x123456789abc, 0, 6);
            return { v: b.readUIntLE(0, 6) };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    // 0x123456789abc = 20015998343868
    assert!(
        r.json.contains(r#""v":20015998343868"#),
        "got: {}",
        r.json
    );
}

#[test]
fn big_int_64_be_round_trip() {
    let r = dispatch(
        m(r#"
        import { Buffer } from "node:buffer";
        export function test() {
            const b = Buffer.alloc(8);
            b.writeBigInt64BE(0x12345678n, 0);
            return { v: b.readBigInt64BE(0).toString() };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""v":"305419896""#), "got: {}", r.json);
}
