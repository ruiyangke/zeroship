//! `node:os` — platform / arch / cpus / EOL / constants assertions.
//! Wave #192.

mod common;
use common::{dispatch, m};

#[test]
fn platform_and_arch_resolve() {
    let r = dispatch(
        m(r#"
        import { platform, arch } from "node:os";
        export function test() {
            return { platform: platform(), archIsString: typeof arch() === "string" };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""platform":"linux""#), "got: {}", r.json);
    assert!(r.json.contains(r#""archIsString":true"#), "got: {}", r.json);
}

#[test]
fn type_returns_linux() {
    let r = dispatch(
        m(r#"
        import { type } from "node:os";
        export function test() {
            return { type: type() };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""type":"Linux""#), "got: {}", r.json);
}

#[test]
fn cpus_returns_at_least_one() {
    let r = dispatch(
        m(r#"
        import { cpus } from "node:os";
        export function test() {
            const list = cpus();
            const c = list[0];
            return {
                count: list.length,
                hasModel: typeof c.model === "string",
                hasTimes: typeof c.times === "object",
                hasIdleField: typeof c.times.idle === "number",
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    // npm packages probe `.length > 0`.
    assert!(r.json.contains(r#""count":1"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasModel":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasTimes":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasIdleField":true"#), "got: {}", r.json);
}

#[test]
fn eol_is_newline() {
    let r = dispatch(
        m(r#"
        import { EOL } from "node:os";
        export function test() {
            return { eol: EOL, len: EOL.length };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""eol":"\n""#), "got: {}", r.json);
    assert!(r.json.contains(r#""len":1"#), "got: {}", r.json);
}

#[test]
fn constants_signals_sigterm() {
    let r = dispatch(
        m(r#"
        import { constants } from "node:os";
        export function test() {
            return {
                sigint: constants.signals.SIGINT,
                sigterm: constants.signals.SIGTERM,
                sigkill: constants.signals.SIGKILL,
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""sigint":2"#), "got: {}", r.json);
    assert!(r.json.contains(r#""sigterm":15"#), "got: {}", r.json);
    assert!(r.json.contains(r#""sigkill":9"#), "got: {}", r.json);
}

#[test]
fn endianness_le() {
    let r = dispatch(
        m(r#"
        import { endianness } from "node:os";
        export function test() {
            return { e: endianness() };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""e":"LE""#), "got: {}", r.json);
}

#[test]
fn user_info_shape() {
    let r = dispatch(
        m(r#"
        import { userInfo } from "node:os";
        export function test() {
            const u = userInfo();
            return {
                hasUsername: typeof u.username === "string",
                hasUid: typeof u.uid === "number",
                hasHomedir: typeof u.homedir === "string",
                shellIsNull: u.shell === null,
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""hasUsername":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasUid":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasHomedir":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""shellIsNull":true"#), "got: {}", r.json);
}

#[test]
fn uptime_is_nonnegative() {
    let r = dispatch(
        m(r#"
        import { uptime } from "node:os";
        export function test() {
            return { ok: uptime() >= 0 };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""ok":true"#), "got: {}", r.json);
}

#[test]
fn loadavg_returns_three_zeros() {
    let r = dispatch(
        m(r#"
        import { loadavg } from "node:os";
        export function test() {
            const la = loadavg();
            return { len: la.length, allZero: la.every(v => v === 0) };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""len":3"#), "got: {}", r.json);
    assert!(r.json.contains(r#""allZero":true"#), "got: {}", r.json);
}

#[test]
fn hostname_and_paths() {
    let r = dispatch(
        m(r#"
        import { hostname, homedir, tmpdir } from "node:os";
        export function test() {
            return {
                host: hostname(),
                home: homedir(),
                tmp: tmpdir(),
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""host":"zeroship-worker""#), "got: {}", r.json);
    assert!(r.json.contains(r#""home":"/""#), "got: {}", r.json);
    assert!(r.json.contains(r#""tmp":"/tmp""#), "got: {}", r.json);
}

#[test]
fn default_import_returns_namespace() {
    let r = dispatch(
        m(r#"
        import os from "node:os";
        export function test() {
            return {
                hasPlatform: typeof os.platform === "function",
                hasCpus: typeof os.cpus === "function",
                hasEOL: typeof os.EOL === "string",
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""hasPlatform":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasCpus":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasEOL":true"#), "got: {}", r.json);
}
