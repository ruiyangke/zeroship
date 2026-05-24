#!/usr/bin/env python3
"""T-8b-stress: polling-aware snapshot/wake stress driver.

C-7-LT migrated the admin wake endpoint to async polling. This rewrite
replaces the legacy synchronous `POST /admin/sandboxes/{id}/wake` with
the documented two-step contract:

  1. POST /admin/sandboxes/{id}/wake             → 202 { wake_id, state, ... }
  2. GET  /admin/sandboxes/{id}/wake/{wake_id}   → 202 (intermediate) | 200 ok/failed

Lifecycle measured per cycle: CREATE → SNAPSHOT → WAKE (post+poll) → STOP.

Targets the controller's local listener on http://127.0.0.1:9091 and
uses the sandbox bearer for tenant calls + the admin bearer for
snapshot/wake.
"""
import argparse
import json
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor

from typed_id import gen as tid_gen


def http_call(method, url, token, body=None, timeout=120):
    data = body.encode() if isinstance(body, str) else body
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Authorization", f"Bearer {token}")
    if data is not None:
        req.add_header("Content-Type", "application/json")
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            payload = resp.read().decode("utf-8", errors="replace")
            return resp.status, payload, time.monotonic() - t0
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", errors="replace"), time.monotonic() - t0
    except Exception as e:
        return 0, f"{type(e).__name__}: {e}", time.monotonic() - t0


def wake_post_then_poll(base_url, admin_token, sid, budget_s, poll_interval_s):
    """POST the wake, then poll the GET endpoint until a terminal state."""
    out = {
        "wake_post_code": None,
        "wake_post_ms": None,
        "wake_post_body": None,
        "wake_id": None,
        "wake_replay": None,
        "wake_poll_count": 0,
        "wake_poll_states": [],
        "wake_total_ms": None,
        "wake_terminal_state": None,
        "wake_terminal_code": None,
        "wake_error_code": None,
        "wake_error_message": None,
        "wake_agent_url": None,
        "wake_ready_at": None,
    }

    t0 = time.monotonic()
    code, body, dt = http_call(
        "POST",
        f"{base_url}/admin/sandboxes/{sid}/wake",
        admin_token,
        b"{}",
        timeout=30,
    )
    out["wake_post_code"] = code
    out["wake_post_ms"] = dt * 1000
    if code != 202:
        out["wake_post_body"] = body[:400]
        out["wake_total_ms"] = (time.monotonic() - t0) * 1000
        return out
    try:
        post_j = json.loads(body)
    except Exception:
        out["wake_post_body"] = "JSON-PARSE-FAIL: " + body[:200]
        out["wake_total_ms"] = (time.monotonic() - t0) * 1000
        return out
    wake_id = post_j.get("wake_id")
    out["wake_id"] = wake_id
    out["wake_replay"] = bool(post_j.get("replay"))
    if not wake_id:
        out["wake_post_body"] = "NO-WAKE-ID: " + body[:200]
        out["wake_total_ms"] = (time.monotonic() - t0) * 1000
        return out

    # Poll loop
    poll_url = f"{base_url}/admin/sandboxes/{sid}/wake/{wake_id}"
    deadline = t0 + budget_s
    last_state = post_j.get("state", "pending")
    out["wake_poll_states"].append(last_state)
    while time.monotonic() < deadline:
        time.sleep(poll_interval_s)
        pcode, pbody, _pdt = http_call("GET", poll_url, admin_token, None, timeout=15)
        out["wake_poll_count"] += 1
        try:
            pj = json.loads(pbody)
        except Exception:
            pj = {}
        state = pj.get("state") or "?"
        if not out["wake_poll_states"] or out["wake_poll_states"][-1] != state:
            out["wake_poll_states"].append(state)
        if pcode == 200:
            # Terminal — either ok or §10.0 failed envelope (still 200)
            out["wake_terminal_code"] = pcode
            out["wake_terminal_state"] = state
            out["wake_total_ms"] = (time.monotonic() - t0) * 1000
            if state == "ok":
                out["wake_agent_url"] = pj.get("agent_url")
                out["wake_ready_at"] = pj.get("ready_at")
            else:
                # Failed envelope — `error` is the wire_code, `message` is the message.
                out["wake_error_code"] = pj.get("error")
                out["wake_error_message"] = pj.get("message")
            return out
        if pcode == 202:
            continue
        # Anything else (404, 5xx) — bail
        out["wake_terminal_code"] = pcode
        out["wake_terminal_state"] = state or "?"
        out["wake_error_code"] = pj.get("error") or f"http_{pcode}"
        out["wake_error_message"] = (pj.get("message") or pbody[:200])
        out["wake_total_ms"] = (time.monotonic() - t0) * 1000
        return out

    # Budget exhausted
    out["wake_terminal_code"] = 0
    out["wake_terminal_state"] = "timeout"
    out["wake_error_code"] = "client_poll_timeout"
    out["wake_total_ms"] = (time.monotonic() - t0) * 1000
    return out


def one_cycle(base_url, sandbox_token, admin_token, idx, wake_budget_s, poll_interval_s):
    """CREATE → SNAPSHOT → WAKE (POST + poll) → STOP."""
    user_id = tid_gen("usr")
    project_id = tid_gen("prj")
    res = {"idx": idx, "user_id": user_id, "project_id": project_id}

    # CREATE
    code, body, dt = http_call(
        "POST",
        f"{base_url}/sandboxes",
        sandbox_token,
        json.dumps({"user_id": user_id, "project_id": project_id}),
        timeout=180,
    )
    res["create_code"] = code
    res["create_ms"] = dt * 1000
    if code not in (200, 201):
        res["create_body"] = body[:400]
        return res
    try:
        sid = json.loads(body).get("sandbox_id")
    except Exception:
        res["create_body"] = "JSON-PARSE-FAIL: " + body[:200]
        return res
    res["sandbox_id"] = sid

    # SNAPSHOT (admin) — sync `wait:true`; sub-30 s on the smoke profile.
    code, body, dt = http_call(
        "POST",
        f"{base_url}/admin/sandboxes/{sid}/snapshot",
        admin_token,
        json.dumps({"wait": True}),
        timeout=120,
    )
    res["snapshot_code"] = code
    res["snapshot_ms"] = dt * 1000
    if code != 200:
        res["snapshot_body"] = body[:400]
        # cleanup best-effort
        http_call(
            "DELETE",
            f"{base_url}/sandboxes/{sid}?user_id={user_id}",
            sandbox_token,
            timeout=60,
        )
        return res

    # WAKE — POST + poll
    wk = wake_post_then_poll(base_url, admin_token, sid, wake_budget_s, poll_interval_s)
    res.update(wk)

    # STOP — fire regardless of wake outcome (cleanup is mandatory)
    code, body, dt = http_call(
        "DELETE",
        f"{base_url}/sandboxes/{sid}?user_id={user_id}",
        sandbox_token,
        timeout=120,
    )
    res["stop_code"] = code
    res["stop_ms"] = dt * 1000
    if code not in (200, 204):
        res["stop_body"] = body[:300]
    return res


def percentile(values, p):
    if not values:
        return None
    s = sorted(values)
    k = int(round((len(s) - 1) * p / 100))
    return s[k]


def summarize(results, label):
    print(f"\n=== {label} (N={len(results)}) ===")

    def phase(name, ok_pred, ms_key, denom_pred=lambda r: True):
        denom = [r for r in results if denom_pred(r)]
        ok = [r for r in denom if ok_pred(r)]
        ms = [r[ms_key] for r in ok if isinstance(r.get(ms_key), (int, float))]
        print(f"{name} OK: {len(ok)}/{len(denom)}")
        if ms:
            print(
                f"  {name.lower()} p50/p95/p99/max ms: "
                f"{percentile(ms,50):.0f} / {percentile(ms,95):.0f} / "
                f"{percentile(ms,99):.0f} / {max(ms):.0f}"
            )

    phase("CREATE", lambda r: r.get("create_code") in (200, 201) and r.get("sandbox_id"), "create_ms")
    phase(
        "SNAPSHOT",
        lambda r: r.get("snapshot_code") == 200,
        "snapshot_ms",
        denom_pred=lambda r: r.get("sandbox_id") is not None,
    )
    phase(
        "WAKE",
        lambda r: r.get("wake_terminal_state") == "ok",
        "wake_total_ms",
        denom_pred=lambda r: r.get("snapshot_code") == 200,
    )
    phase(
        "STOP",
        lambda r: r.get("stop_code") in (200, 204),
        "stop_ms",
        denom_pred=lambda r: r.get("sandbox_id") is not None,
    )

    # Wake-failure breakdown by error_code
    wake_fails = [
        r for r in results
        if r.get("snapshot_code") == 200 and r.get("wake_terminal_state") != "ok"
    ]
    if wake_fails:
        print(f"FAILED WAKES: {len(wake_fails)}")
        seen = {}
        for f in wake_fails:
            key = (
                f.get("wake_terminal_code"),
                f.get("wake_terminal_state"),
                f.get("wake_error_code"),
                str(f.get("wake_error_message", ""))[:120],
            )
            seen[key] = seen.get(key, 0) + 1
        for (tc, ts, ec, msg), n in sorted(seen.items(), key=lambda x: -x[1]):
            print(f"  [{n}x] code={tc} state={ts} error={ec}: {msg}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base-url", default="http://127.0.0.1:9091")
    ap.add_argument("--token-file", default="/etc/zeroship/sandbox-token")
    ap.add_argument("--admin-token-file", default="/etc/zeroship/sandbox-admin-token")
    ap.add_argument("--concurrency", type=int, default=1)
    ap.add_argument("--cycles", type=int, default=20)
    ap.add_argument("--label", default="t8b-stress")
    ap.add_argument(
        "--wake-budget",
        type=int,
        default=180,
        help="Per-cycle wake polling budget (seconds).",
    )
    ap.add_argument(
        "--poll-interval",
        type=float,
        default=1.0,
        help="Wake GET-poll interval (seconds).",
    )
    args = ap.parse_args()

    with open(args.token_file) as f:
        sandbox_token = f.read().strip()
    with open(args.admin_token_file) as f:
        admin_token = f.read().strip()

    total = args.concurrency * args.cycles
    print(
        f"# {args.label}: concurrency={args.concurrency}, "
        f"cycles_per_worker={args.cycles}, total={total}"
    )
    print(
        f"# base_url={args.base_url}  "
        f"wake_budget={args.wake_budget}s  poll_interval={args.poll_interval}s"
    )

    results = []
    t_run = time.monotonic()
    with ThreadPoolExecutor(max_workers=args.concurrency) as ex:
        futs = [
            ex.submit(
                one_cycle,
                args.base_url,
                sandbox_token,
                admin_token,
                i,
                args.wake_budget,
                args.poll_interval,
            )
            for i in range(total)
        ]
        for fut in futs:
            try:
                results.append(fut.result(timeout=args.wake_budget + 600))
            except Exception as e:
                results.append({"error": f"{type(e).__name__}: {e}"})
    elapsed = time.monotonic() - t_run
    print(f"# elapsed: {elapsed:.1f}s")
    summarize(results, args.label)

    print("\n=== RAW_JSON_BEGIN ===")
    print(json.dumps(results, default=str))
    print("=== RAW_JSON_END ===")


if __name__ == "__main__":
    main()
