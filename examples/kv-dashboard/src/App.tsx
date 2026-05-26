import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  acquireLease,
  clearDemo,
  clearLease,
  createSession,
  deleteSession,
  deleteStringValue,
  expireStringValue,
  getMemo,
  getQuote,
  getSnapshot,
  hitRateLimit,
  listKeys,
  persistStringValue,
  recordVisit,
  setCheckoutFlag,
  setStringValue,
  type DashboardSnapshot,
  type KeysPage,
  type LeaseResult,
  type MemoResult,
  type QuoteResult,
  type RateLimitResult,
} from "./index";

type NoticeKind = "ok" | "error";
type Notice = { kind: NoticeKind; text: string } | null;
type PendingMap = Record<string, boolean>;

const EMPTY_SNAPSHOT: DashboardSnapshot = {
  visits: 0,
  checkoutEnabled: false,
  lease: null,
  sessions: [],
  sessionsTruncated: false,
  text: { value: null, has: false, ttlMs: null },
  keys: [],
  generatedAt: 0,
};

function elapsed(ms: number): string {
  if (!ms) return "-";
  const delta = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (delta < 60) return `${delta}s`;
  const minutes = Math.round(delta / 60);
  if (minutes < 60) return `${minutes}m`;
  return `${Math.round(minutes / 60)}h`;
}

function ttl(ms: number | null): string {
  if (ms == null) return "none";
  if (ms < 1000) return "<1s";
  const seconds = Math.ceil(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  return `${Math.ceil(seconds / 60)}m`;
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function sessionCount(snapshot: DashboardSnapshot): string {
  return `${snapshot.sessions.length}${snapshot.sessionsTruncated ? "+" : ""}`;
}

export function App() {
  const [snapshot, setSnapshot] = useState<DashboardSnapshot>(EMPTY_SNAPSHOT);
  const [keysPage, setKeysPage] = useState<KeysPage>({ keys: [], cursor: null });
  const [keyCursor, setKeyCursor] = useState<string | null>(null);
  const [keyPrefix, setKeyPrefix] = useState("");
  const [notice, setNotice] = useState<Notice>(null);
  const [pending, setPending] = useState<PendingMap>({});
  const [actor, setActor] = useState("guest");
  const [rate, setRate] = useState<RateLimitResult | null>(null);
  const [sku, setSku] = useState("starter");
  const [quote, setQuote] = useState<QuoteResult | null>(null);
  const [memoLabel, setMemoLabel] = useState("welcome");
  const [memo, setMemo] = useState<MemoResult | null>(null);
  const [textDraft, setTextDraft] = useState("hello from kv");
  const [owner, setOwner] = useState("operator-a");
  const [leaseResult, setLeaseResult] = useState<LeaseResult | null>(null);
  const [sessionName, setSessionName] = useState("Preview user");
  const noticeTimer = useRef<number | null>(null);

  const setActionPending = useCallback((id: string, active: boolean) => {
    setPending((current) => {
      if (Boolean(current[id]) === active) return current;
      const next = { ...current };
      if (active) next[id] = true;
      else delete next[id];
      return next;
    });
  }, []);

  const isPending = useCallback((id: string) => Boolean(pending[id]), [pending]);

  const show = useCallback((kind: NoticeKind, text: string) => {
    const next = { kind, text };
    if (noticeTimer.current !== null) window.clearTimeout(noticeTimer.current);
    setNotice(next);
    noticeTimer.current = window.setTimeout(() => {
      setNotice((current) => (current === next ? null : current));
      noticeTimer.current = null;
    }, 2600);
  }, []);

  useEffect(() => {
    return () => {
      if (noticeTimer.current !== null) window.clearTimeout(noticeTimer.current);
    };
  }, []);

  const refresh = useCallback(async () => {
    const next = await getSnapshot();
    setSnapshot(next);
    const page = await listKeys({ prefix: keyPrefix, cursor: null, limit: 12 });
    setKeyCursor(null);
    setKeysPage(page);
  }, [keyPrefix]);

  const run = useCallback(
    async <T,>(
      action: string,
      work: () => T | Promise<T>,
      after?: (value: T) => void,
      options: { refresh?: boolean } = {},
    ) => {
      setActionPending(action, true);
      try {
        const value = await work();
        after?.(value);
        if (options.refresh !== false) await refresh();
      } catch (error) {
        show("error", errorText(error));
      } finally {
        setActionPending(action, false);
      }
    },
    [refresh, setActionPending, show],
  );

  useEffect(() => {
    let cancelled = false;
    setActionPending("boot", true);
    void (async () => {
      try {
        const next = await recordVisit();
        if (cancelled) return;
        setSnapshot(next);
        setKeysPage(await listKeys({ prefix: "", cursor: null, limit: 12 }));
      } catch (error) {
        if (!cancelled) show("error", errorText(error));
      } finally {
        if (!cancelled) setActionPending("boot", false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [setActionPending, show]);

  const sortedKeys = useMemo(() => keysPage.keys.slice().sort(), [keysPage.keys]);

  const firstKeys = () => {
    void run(
      "keys",
      () => listKeys({ prefix: keyPrefix, cursor: null, limit: 12 }),
      (page) => {
        setKeyCursor(null);
        setKeysPage(page);
      },
      { refresh: false },
    );
  };

  const nextKeys = () => {
    if (!keysPage.cursor) return;
    void run(
      "keys",
      () => listKeys({ prefix: keyPrefix, cursor: keysPage.cursor, limit: 12 }),
      (page) => {
        setKeyCursor(keysPage.cursor);
        setKeysPage(page);
      },
      { refresh: false },
    );
  };

  return (
    <main className="shell">
      <header className="topbar">
        <div>
          <h1>KV Dashboard</h1>
          <div className="subtle">redb or Redis - same `@zeroship/kv` surface</div>
        </div>
        <div className="actions">
          <button
            type="button"
            className="secondary"
            disabled={isPending("boot") || isPending("refresh")}
            onClick={() => void run("refresh", refresh, undefined, { refresh: false })}
          >
            Refresh
          </button>
          <button
            type="button"
            className="danger"
            disabled={isPending("clear")}
            onClick={() => void run("clear", clearDemo, (r) => show("ok", `Deleted ${r.deleted} keys`))}
          >
            Clear
          </button>
        </div>
      </header>

      {notice && <div className={`notice ${notice.kind}`}>{notice.text}</div>}

      <section className="metrics" aria-label="KV summary">
        <div>
          <span>Visits</span>
          <strong>{snapshot.visits}</strong>
        </div>
        <div>
          <span>Checkout</span>
          <strong>{snapshot.checkoutEnabled ? "enabled" : "disabled"}</strong>
        </div>
        <div>
          <span>Sessions</span>
          <strong>{sessionCount(snapshot)}</strong>
        </div>
        <div>
          <span>Keys</span>
          <strong>{snapshot.keys.length}</strong>
        </div>
      </section>

      <section className="grid">
        <form
          className="panel"
          onSubmit={(e) => {
            e.preventDefault();
            void run("flag", () => setCheckoutFlag({ enabled: !snapshot.checkoutEnabled }), setSnapshot);
          }}
        >
          <div className="panelHead">
            <h2>Feature Flag</h2>
            <span className={snapshot.checkoutEnabled ? "pill on" : "pill"}>
              {snapshot.checkoutEnabled ? "on" : "off"}
            </span>
          </div>
          <div className="row">
            <span>checkout</span>
            <button type="submit" disabled={isPending("flag")}>
              {snapshot.checkoutEnabled ? "Disable" : "Enable"}
            </button>
          </div>
        </form>

        <form
          className="panel"
          onSubmit={(e) => {
            e.preventDefault();
            void run("rate", () => hitRateLimit({ actor }), setRate);
          }}
        >
          <div className="panelHead">
            <h2>Rate Window</h2>
            <span className={rate?.allowed === false ? "pill blocked" : "pill on"}>
              {rate?.allowed === false ? "blocked" : "open"}
            </span>
          </div>
          <label>
            Actor
            <input value={actor} onChange={(e) => setActor(e.currentTarget.value)} />
          </label>
          <button type="submit" disabled={isPending("rate")}>Hit</button>
          <dl>
            <div><dt>count</dt><dd>{rate?.count ?? 0}</dd></div>
            <div><dt>remaining</dt><dd>{rate?.remaining ?? 5}</dd></div>
            <div><dt>reset</dt><dd>{ttl(rate?.resetMs ?? null)}</dd></div>
          </dl>
        </form>

        <form
          className="panel"
          onSubmit={(e) => {
            e.preventDefault();
            void run("string.set", () => setStringValue({ value: textDraft, ttlMs: 120_000 }));
          }}
        >
          <div className="panelHead">
            <h2>String + TTL</h2>
            <span className={snapshot.text.has ? "pill on" : "pill"}>{snapshot.text.has ? "set" : "empty"}</span>
          </div>
          <label>
            Value
            <input value={textDraft} onChange={(e) => setTextDraft(e.currentTarget.value)} />
          </label>
          <div className="buttonRow">
            <button type="submit" disabled={isPending("string.set")}>Set</button>
            <button
              type="button"
              className="secondary"
              disabled={isPending("string.expire")}
              onClick={() => void run("string.expire", () => expireStringValue({ ttlMs: 120_000 }))}
            >
              Expire
            </button>
            <button
              type="button"
              className="secondary"
              disabled={isPending("string.persist")}
              onClick={() => void run("string.persist", () => persistStringValue())}
            >
              Persist
            </button>
            <button
              type="button"
              className="ghost"
              disabled={isPending("string.delete")}
              onClick={() => void run("string.delete", () => deleteStringValue())}
            >
              Delete
            </button>
          </div>
          <dl>
            <div><dt>getString</dt><dd>{snapshot.text.value ?? "-"}</dd></div>
            <div><dt>has</dt><dd>{snapshot.text.has ? "true" : "false"}</dd></div>
            <div><dt>ttl</dt><dd>{ttl(snapshot.text.ttlMs)}</dd></div>
          </dl>
        </form>

        <form
          className="panel"
          onSubmit={(e) => {
            e.preventDefault();
            void run("memo", () => getMemo({ label: memoLabel }), setMemo);
          }}
        >
          <div className="panelHead">
            <h2>getOrSet</h2>
            <span className={memo?.source === "hit" ? "pill on" : "pill"}>{memo?.source ?? "cold"}</span>
          </div>
          <label>
            Label
            <input value={memoLabel} onChange={(e) => setMemoLabel(e.currentTarget.value)} />
          </label>
          <button type="submit" disabled={isPending("memo")}>Memoize</button>
          <dl>
            <div><dt>nonce</dt><dd>{memo?.value.nonce ?? "-"}</dd></div>
            <div><dt>ttl</dt><dd>{ttl(memo?.ttlMs ?? null)}</dd></div>
            <div><dt>age</dt><dd>{memo ? elapsed(memo.value.builtAt) : "-"}</dd></div>
          </dl>
        </form>

        <form
          className="panel"
          onSubmit={(e) => {
            e.preventDefault();
            void run("quote", () => getQuote({ sku }), setQuote);
          }}
        >
          <div className="panelHead">
            <h2>Cache</h2>
            <span className={quote?.source === "hit" ? "pill on" : "pill"}>{quote?.source ?? "cold"}</span>
          </div>
          <label>
            SKU
            <input value={sku} onChange={(e) => setSku(e.currentTarget.value)} />
          </label>
          <button type="submit" disabled={isPending("quote")}>Quote</button>
          <dl>
            <div><dt>price</dt><dd>{quote ? `$${quote.quote.price.toFixed(2)}` : "-"}</dd></div>
            <div><dt>ttl</dt><dd>{ttl(quote?.ttlMs ?? null)}</dd></div>
            <div><dt>age</dt><dd>{quote ? elapsed(quote.quote.generatedAt) : "-"}</dd></div>
          </dl>
        </form>

        <form
          className="panel"
          onSubmit={(e) => {
            e.preventDefault();
            void run("lease.acquire", () => acquireLease({ owner }), setLeaseResult);
          }}
        >
          <div className="panelHead">
            <h2>Ephemeral Lease</h2>
            <span className={snapshot.lease ? "pill blocked" : "pill on"}>{snapshot.lease ? "held" : "free"}</span>
          </div>
          <label>
            Owner
            <input value={owner} onChange={(e) => setOwner(e.currentTarget.value)} />
          </label>
          <div className="buttonRow">
            <button type="submit" disabled={isPending("lease.acquire")}>Acquire</button>
            <button
              type="button"
              className="secondary"
              disabled={isPending("lease.clear")}
              onClick={() => void run("lease.clear", () => clearLease(), setLeaseResult)}
            >
              Reset
            </button>
          </div>
          <dl>
            <div><dt>owner</dt><dd>{snapshot.lease?.owner ?? "-"}</dd></div>
            <div><dt>ttl</dt><dd>{ttl(snapshot.lease?.ttlMs ?? null)}</dd></div>
            <div>
              <dt>last</dt>
              <dd>
                {leaseResult
                  ? leaseResult.acquired
                    ? "acquired"
                    : leaseResult.lease
                      ? "held"
                      : "cleared"
                  : "-"}
              </dd>
            </div>
          </dl>
        </form>

        <form
          className="panel sessions"
          onSubmit={(e) => {
            e.preventDefault();
            void run("session.create", () => createSession({ name: sessionName }), (s) => show("ok", `Created ${s.token}`));
          }}
        >
          <div className="panelHead">
            <h2>Sessions</h2>
            <span className="pill">{sessionCount(snapshot)}</span>
          </div>
          <label>
            Name
            <input value={sessionName} onChange={(e) => setSessionName(e.currentTarget.value)} />
          </label>
          <button type="submit" disabled={isPending("session.create")}>Create</button>
          <ul className="list">
            {snapshot.sessions.map((session) => (
              <li key={session.token}>
                <span>
                  <strong>{session.name}</strong>
                  <small>{session.token} - {ttl(session.ttlMs)}</small>
                </span>
                <button
                  type="button"
                  className="ghost"
                  disabled={isPending(`session.delete.${session.token}`)}
                  onClick={() => void run(`session.delete.${session.token}`, () => deleteSession({ token: session.token }))}
                >
                  Delete
                </button>
              </li>
            ))}
            {snapshot.sessionsTruncated && <li className="empty">Showing first {snapshot.sessions.length} sessions</li>}
            {snapshot.sessions.length === 0 && <li className="empty">No active sessions</li>}
          </ul>
        </form>

        <form
          className="panel keyspace"
          onSubmit={(e) => {
            e.preventDefault();
            firstKeys();
          }}
        >
          <div className="panelHead">
            <h2>Keyspace</h2>
            <span className="pill">{keyCursor ? "page" : "root"}</span>
          </div>
          <label>
            Prefix
            <input value={keyPrefix} onChange={(e) => setKeyPrefix(e.currentTarget.value)} placeholder="session:" />
          </label>
          <div className="buttonRow">
            <button type="submit" disabled={isPending("keys")}>List</button>
            <button type="button" className="secondary" disabled={isPending("keys") || !keysPage.cursor} onClick={nextKeys}>
              Next
            </button>
          </div>
          <ul className="keys">
            {sortedKeys.map((key) => <li key={key}>{key}</li>)}
            {sortedKeys.length === 0 && <li className="empty">No keys</li>}
          </ul>
        </form>
      </section>
    </main>
  );
}
