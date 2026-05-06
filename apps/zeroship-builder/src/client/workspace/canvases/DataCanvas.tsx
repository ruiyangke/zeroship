// ─── DataCanvas — five-subtab DB view (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §9.3) ───────────────
//
// Five subtab pills inside one canvas:
//   1. Tables    — list of relations from listTables; row click opens
//                  a paginated row browser (getTableRows).
//   2. Schema    — placeholder until the visualizer is wired.
//   3. Indexes   — flat list from listIndexes.
//   4. Migrations— timeline list from listMigrations.
//   5. Backups   — list from listBackups + a "trigger backup" button
//                  that calls triggerBackup.
//
// All data flows through the in-memory stubs in `src/server/agents.ts`
// Real pg_catalog introspection and per-app
// schema reads / migration log / backup trigger all need control-plane
// work — every "Coming soon" copy points at the right ISSUE.

import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  listTables,
  getTableRows,
  listIndexes,
  listMigrations,
  listBackups,
  triggerBackup,
  type TableSummary,
  type IndexInfo,
  type MigrationEntry,
  type MigrationStatus,
  type BackupEntry,
} from "../../api";
import { StampButton } from "../../components/StampButton";

export interface DataCanvasProps {
  appId: string;
}

type Subtab = "tables" | "schema" | "indexes" | "migrations" | "backups";

const SUBTABS: ReadonlyArray<{ id: Subtab; label: string }> = [
  { id: "tables", label: "Tables" },
  { id: "schema", label: "Schema" },
  { id: "indexes", label: "Indexes" },
  { id: "migrations", label: "Migrations" },
  { id: "backups", label: "Backups" },
];

export function DataCanvas({ appId }: DataCanvasProps) {
  const [active, setActive] = useState<Subtab>("tables");

  return (
    <div data-testid="data-canvas" className="h-full overflow-auto bg-paper">
      <div className="max-w-[960px] mx-auto px-4 sm:px-8 lg:px-12 py-6 sm:py-10">
        <header className="mb-6">
          <h1 className="font-serif italic font-medium text-[28px] m-0 mb-1">
            Data
          </h1>
          <p className="font-serif text-[14px] text-ink-soft leading-[1.55]">
            Tables, indexes, migrations, backups. Read-only in this
            build while the live catalog, index, migration, and backup
            plumbing lands.
          </p>
        </header>

        <div className="flex items-center gap-1.5 mb-6" data-testid="data-subtabs">
          {SUBTABS.map((s) => (
            <button
              key={s.id}
              type="button"
              onClick={() => setActive(s.id)}
              data-testid={`data-subtab:${s.id}`}
              className={
                "inline-flex items-center rounded-full font-sans px-2.5 py-1 text-[11px] cursor-pointer transition-colors " +
                (active === s.id
                  ? "bg-tomato text-paper border-0"
                  : "bg-transparent text-ink-soft border border-rule hover:border-ink hover:text-ink")
              }
            >
              {s.label}
            </button>
          ))}
        </div>

        {active === "tables" && <TablesPane appId={appId} />}
        {active === "schema" && <SchemaPane />}
        {active === "indexes" && <IndexesPane appId={appId} />}
        {active === "migrations" && <MigrationsPane appId={appId} />}
        {active === "backups" && <BackupsPane appId={appId} />}
      </div>
    </div>
  );
}

// ─── 1 · Tables ─────────────────────────────────────────────────

function TablesPane({ appId }: { appId: string }) {
  const { data, isLoading, error } = useQuery({
    queryKey: ["data-tables", appId],
    queryFn: () => listTables({ appId }),
    retry: false,
  });
  const [openTable, setOpenTable] = useState<string | null>(null);

  if (openTable) {
    return (
      <RowBrowser
        appId={appId}
        tableName={openTable}
        onBack={() => setOpenTable(null)}
      />
    );
  }

  return (
    <section data-testid="data-subtab-content:tables">
      {isLoading && (
        <div className="font-serif italic text-pencil py-2">loading…</div>
      )}
      {error && (
        <div className="font-serif italic text-tomato py-2">
          couldn't load tables
        </div>
      )}
      {data && data.tables.length === 0 && (
        <div
          data-testid="data-tables-empty"
          className="font-serif italic text-pencil py-8 text-center border border-dashed border-rule"
        >
          Empty schema — Builder hasn't seeded any tables yet. They'll show up
          here once your project writes the first row.
        </div>
      )}
      {data && data.tables.length > 0 && (
        <div className="border border-rule-2">
          <div
            className="grid items-center gap-4 px-4 py-2 border-b border-rule-2 bg-paper-2/40"
            style={{ gridTemplateColumns: "1fr 100px 120px 140px" }}
          >
            <div className="label-uc">Name</div>
            <div className="label-uc text-right">Rows</div>
            <div className="label-uc text-right">Size</div>
            <div className="label-uc text-right">Updated</div>
          </div>
          {data.tables.map((t) => (
            <TableRow key={t.name} table={t} onOpen={() => setOpenTable(t.name)} />
          ))}
        </div>
      )}
      <div className="mt-3 font-serif italic text-[12px] text-pencil">
        Live `pg_catalog` introspection is not wired yet.
      </div>
    </section>
  );
}

function TableRow({
  table,
  onOpen,
}: {
  table: TableSummary;
  onOpen: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onOpen}
      data-testid={`data-table-row:${table.name}`}
      className="w-full grid items-center gap-4 px-4 py-3 border-b border-rule-2 last:border-b-0 bg-transparent border-l-0 border-r-0 border-t-0 cursor-pointer text-left hover:bg-paper-2/60"
      style={{ gridTemplateColumns: "1fr 100px 120px 140px" }}
    >
      <span className="font-mono text-[13px] text-ink truncate">{table.name}</span>
      <span className="font-mono text-[12.5px] text-ink-soft text-right">
        {table.row_count.toLocaleString()}
      </span>
      <span className="font-mono text-[12.5px] text-ink-soft text-right">
        {fmtBytes(table.size_bytes)}
      </span>
      <span className="font-serif italic text-[12.5px] text-pencil text-right">
        {relativeTime(table.updated_at)}
      </span>
    </button>
  );
}

function RowBrowser({
  appId,
  tableName,
  onBack,
}: {
  appId: string;
  tableName: string;
  onBack: () => void;
}) {
  const [offset, setOffset] = useState(0);
  const limit = 50;
  const { data, isLoading } = useQuery({
    queryKey: ["data-table-rows", appId, tableName, limit, offset],
    queryFn: () => getTableRows({ appId, tableName, limit, offset }),
    retry: false,
  });

  const total = data?.total ?? 0;
  const hasPrev = offset > 0;
  const hasNext = offset + limit < total;

  return (
    <section data-testid="data-row-browser">
      <header className="flex items-baseline justify-between mb-4">
        <div>
          <button
            type="button"
            onClick={onBack}
            data-testid="data-row-browser-back"
            className="font-serif italic text-[13px] text-tomato bg-transparent border-0 cursor-pointer hover:opacity-80 mb-1"
          >
            ← Tables
          </button>
          <h2 className="font-serif italic font-medium text-[20px] m-0">
            <span className="font-mono text-[18px] text-ink">{tableName}</span>
          </h2>
        </div>
        <div className="font-serif italic text-[12.5px] text-pencil">
          {total === 0
            ? "no rows"
            : `${offset + 1}–${Math.min(offset + limit, total)} of ${total}`}
        </div>
      </header>

      {isLoading && (
        <div className="font-serif italic text-pencil py-2">loading…</div>
      )}

      {data && data.rows.length === 0 && (
        <div
          data-testid="data-rows-empty"
          className="font-serif italic text-pencil py-8 text-center border border-dashed border-rule"
        >
          Empty table — Builder hasn't seeded any rows yet.
        </div>
      )}

      {data && data.rows.length > 0 && (
        <div className="border border-rule-2 overflow-auto">
          <table className="min-w-full border-collapse">
            <thead>
              <tr className="bg-paper-2/40">
                {data.columns.map((col) => (
                  <th
                    key={col}
                    className="label-uc text-left px-3 py-2 border-b border-rule-2 font-sans text-[10px] uppercase tracking-[0.16em] text-pencil"
                  >
                    {col}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {data.rows.map((row) => (
                <tr
                  key={row.id}
                  data-testid={`data-row:${row.id}`}
                  className="border-b border-rule-2 last:border-b-0"
                >
                  {data.columns.map((col) => (
                    <td
                      key={col}
                      className="font-mono text-[12px] text-ink-soft px-3 py-2 truncate max-w-[280px]"
                    >
                      {row.cells[col] ?? ""}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      <div className="mt-3 flex items-center justify-between">
        <div className="font-serif italic text-[12px] text-pencil">
          Pagination over live per-app schemas is not wired yet.
        </div>
        <div className="flex items-center gap-3">
          <button
            type="button"
            disabled={!hasPrev}
            onClick={() => setOffset(Math.max(0, offset - limit))}
            data-testid="data-row-browser-prev"
            className="font-serif italic text-[13px] text-ink-soft bg-transparent border-0 cursor-pointer hover:text-ink disabled:opacity-30 disabled:cursor-not-allowed"
          >
            ← prev
          </button>
          <button
            type="button"
            disabled={!hasNext}
            onClick={() => setOffset(offset + limit)}
            data-testid="data-row-browser-next"
            className="font-serif italic text-[13px] text-ink-soft bg-transparent border-0 cursor-pointer hover:text-ink disabled:opacity-30 disabled:cursor-not-allowed"
          >
            next →
          </button>
        </div>
      </div>
    </section>
  );
}

// ─── 2 · Schema ─────────────────────────────────────────────────

function SchemaPane() {
  return (
    <section data-testid="data-subtab-content:schema">
      <div
        data-testid="data-schema-empty"
        className="border border-rule-2 bg-paper-2/40 py-12 text-center"
      >
        <div className="font-serif italic text-[16px] text-ink-soft mb-1">
          Schema visualizer coming soon.
        </div>
        <div className="font-serif italic text-[12.5px] text-pencil">
          Relationship and schema visualization are not wired yet.
        </div>
      </div>
    </section>
  );
}

// ─── 3 · Indexes ────────────────────────────────────────────────

function IndexesPane({ appId }: { appId: string }) {
  const { data, isLoading } = useQuery({
    queryKey: ["data-indexes", appId],
    queryFn: () => listIndexes({ appId }),
    retry: false,
  });

  return (
    <section data-testid="data-subtab-content:indexes">
      {isLoading && (
        <div className="font-serif italic text-pencil py-2">loading…</div>
      )}
      {data && data.indexes.length === 0 && (
        <div
          data-testid="data-indexes-empty"
          className="font-serif italic text-pencil py-8 text-center border border-dashed border-rule"
        >
          No indexes on record — Builder will add them as the schema grows.
        </div>
      )}
      {data && data.indexes.length > 0 && (
        <div className="border border-rule-2">
          <div
            className="grid items-center gap-4 px-4 py-2 border-b border-rule-2 bg-paper-2/40"
            style={{ gridTemplateColumns: "1.2fr 1.4fr 80px 1fr 100px 120px" }}
          >
            <div className="label-uc">Table</div>
            <div className="label-uc">Index</div>
            <div className="label-uc">Type</div>
            <div className="label-uc">Columns</div>
            <div className="label-uc text-right">Size</div>
            <div className="label-uc text-right">Last used</div>
          </div>
          {data.indexes.map((idx) => (
            <IndexRow key={idx.name} idx={idx} />
          ))}
        </div>
      )}
      <div className="mt-3 font-serif italic text-[12px] text-pencil">
        Live `pg_indexes` introspection is not wired yet.
      </div>
    </section>
  );
}

function IndexRow({ idx }: { idx: IndexInfo }) {
  return (
    <div
      data-testid={`data-index-row:${idx.name}`}
      className="grid items-center gap-4 px-4 py-3 border-b border-rule-2 last:border-b-0"
      style={{ gridTemplateColumns: "1.2fr 1.4fr 80px 1fr 100px 120px" }}
    >
      <span className="font-mono text-[13px] text-ink truncate">{idx.table}</span>
      <span className="font-mono text-[12.5px] text-ink-soft truncate">{idx.name}</span>
      <span className="font-sans text-[10px] uppercase tracking-[0.16em] text-ink-soft border border-rule rounded-full px-2 py-0.5 inline-block w-fit">
        {idx.type}
      </span>
      <span className="font-mono text-[12px] text-ink-soft truncate">
        {idx.columns.join(", ")}
      </span>
      <span className="font-mono text-[12.5px] text-ink-soft text-right">
        {fmtBytes(idx.size_bytes)}
      </span>
      <span className="font-serif italic text-[12.5px] text-pencil text-right">
        {idx.last_used ? relativeTime(idx.last_used) : "never"}
      </span>
    </div>
  );
}

// ─── 4 · Migrations ─────────────────────────────────────────────

function MigrationsPane({ appId }: { appId: string }) {
  const { data, isLoading } = useQuery({
    queryKey: ["data-migrations", appId],
    queryFn: () => listMigrations({ appId }),
    retry: false,
  });

  return (
    <section data-testid="data-subtab-content:migrations">
      {isLoading && (
        <div className="font-serif italic text-pencil py-2">loading…</div>
      )}
      {data && data.migrations.length === 0 && (
        <div
          data-testid="data-migrations-empty"
          className="font-serif italic text-pencil py-8 text-center border border-dashed border-rule"
        >
          No migrations applied yet — once the schema changes, the timeline starts here.
        </div>
      )}
      {data && data.migrations.length > 0 && (
        <div>
          {data.migrations.map((m) => (
            <MigrationRow key={m.id} m={m} />
          ))}
        </div>
      )}
      <div className="mt-3 font-serif italic text-[12px] text-pencil">
        Persistent migration history is not wired yet.
      </div>
    </section>
  );
}

function MigrationRow({ m }: { m: MigrationEntry }) {
  return (
    <div
      data-testid={`data-migration-row:${m.id}`}
      className="grid items-center gap-4 px-4 py-3 border-b border-rule-2"
      style={{ gridTemplateColumns: "16px 1fr auto auto" }}
    >
      <MigrationDot status={m.status} />
      <div>
        <div className="font-serif text-[14px] text-ink">{m.name}</div>
        <div className="font-mono text-[11px] text-pencil mt-0.5">{m.id}</div>
      </div>
      <span className="font-sans text-[10px] uppercase tracking-[0.16em] text-ink-soft border border-rule rounded-full px-2 py-0.5">
        {m.author}
      </span>
      <span className="font-serif italic text-[12.5px] text-pencil">
        {relativeTime(m.at)}
      </span>
    </div>
  );
}

function MigrationDot({ status }: { status: MigrationStatus }) {
  const colour =
    status === "applied"
      ? "bg-ivy"
      : status === "pending"
        ? "bg-pencil"
        : "bg-tomato";
  return (
    <span
      aria-hidden="true"
      title={status}
      className={`size-2.5 rounded-full ${colour} inline-block`}
    />
  );
}

// ─── 5 · Backups ────────────────────────────────────────────────

function BackupsPane({ appId }: { appId: string }) {
  const qc = useQueryClient();
  const { data, isLoading } = useQuery({
    queryKey: ["data-backups", appId],
    queryFn: () => listBackups({ appId }),
    retry: false,
  });

  const trigger = useMutation({
    mutationFn: () => triggerBackup({ appId }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["data-backups", appId] });
    },
  });

  return (
    <section data-testid="data-subtab-content:backups">
      <header className="flex items-baseline justify-between mb-4">
        <div>
          <h2 className="font-serif italic font-medium text-[20px] m-0 mb-1">
            Snapshots
          </h2>
          <p className="font-serif text-[13.5px] text-ink-soft leading-[1.55] m-0">
            Daily auto-snapshots plus on-demand manual ones. Restore is
            not wired yet.
          </p>
        </div>
        <StampButton
          onClick={() => trigger.mutate()}
          loading={trigger.isPending}
          data-testid="data-backup-trigger"
        >
          Trigger backup
        </StampButton>
      </header>

      {isLoading && (
        <div className="font-serif italic text-pencil py-2">loading…</div>
      )}

      {data && data.backups.length === 0 && (
        <div
          data-testid="data-backups-empty"
          className="font-serif italic text-pencil py-8 text-center border border-dashed border-rule"
        >
          No snapshots yet — daily auto-snapshots start once Builder ships v0.1.
        </div>
      )}

      {data && data.backups.length > 0 && (
        <div className="border border-rule-2">
          <div
            className="grid items-center gap-4 px-4 py-2 border-b border-rule-2 bg-paper-2/40"
            style={{ gridTemplateColumns: "16px 1fr 100px 120px 140px" }}
          >
            <span />
            <div className="label-uc">Label</div>
            <div className="label-uc">Kind</div>
            <div className="label-uc text-right">Size</div>
            <div className="label-uc text-right">Taken</div>
          </div>
          {data.backups.map((b) => (
            <BackupRow key={b.id} b={b} />
          ))}
        </div>
      )}

      <div className="mt-3 font-serif italic text-[12px] text-pencil">
        Snapshot export and restore wiring are not connected yet.
      </div>
    </section>
  );
}

function BackupRow({ b }: { b: BackupEntry }) {
  return (
    <div
      data-testid={`data-backup-row:${b.id}`}
      className="grid items-center gap-4 px-4 py-3 border-b border-rule-2 last:border-b-0"
      style={{ gridTemplateColumns: "16px 1fr 100px 120px 140px" }}
    >
      <span
        aria-hidden="true"
        className={
          "size-2.5 rounded-full inline-block " +
          (b.kind === "auto" ? "bg-pencil" : "bg-tomato")
        }
      />
      <div>
        <div className="font-serif text-[14px] text-ink">{b.label}</div>
        <div className="font-mono text-[11px] text-pencil mt-0.5">{b.id}</div>
      </div>
      <span className="font-sans text-[10px] uppercase tracking-[0.16em] text-ink-soft border border-rule rounded-full px-2 py-0.5 inline-block w-fit">
        {b.kind}
      </span>
      <span className="font-mono text-[12.5px] text-ink-soft text-right">
        {fmtBytes(b.size_bytes)}
      </span>
      <span className="font-serif italic text-[12.5px] text-pencil text-right">
        {relativeTime(b.at)}
      </span>
    </div>
  );
}

// ─── helpers ────────────────────────────────────────────────────

function fmtBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) return "—";
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

function relativeTime(iso: string): string {
  const t = new Date(iso).getTime();
  if (!Number.isFinite(t)) return "—";
  const diff = Date.now() - t;
  if (diff < 60_000) return "just now";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
  return `${Math.floor(diff / 86_400_000)}d ago`;
}
