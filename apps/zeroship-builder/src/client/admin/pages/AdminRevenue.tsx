// ─── Admin Revenue — MRR + per-creator + platform fee ──────────
//
// Placeholder figures until billing tables land. Hand-ruled bar
// chart in tomato to match the ledger aesthetic.

import { AdminShell } from "../AdminShell";

export function AdminRevenue() {
  // 30-day pretend MRR series, climbing
  const series = Array.from({ length: 30 }, (_, i) => 35 + i * 2 + (i === 28 ? 6 : 0));
  const max = Math.max(...series);

  return (
    <AdminShell pageLabel="revenue">
      <h1 className="font-serif font-medium text-[40px] leading-[1.0] -tracking-[0.02em] mb-2">
        [Admin] <em className="italic text-tomato">Revenue</em>.
      </h1>
      <p className="font-serif italic text-[15px] text-ink-soft mb-7">
        MRR over time, per-creator breakdown, platform fee accumulation.
      </p>

      <div className="grid gap-12" style={{ gridTemplateColumns: "2fr 1fr" }}>
        <section>
          <div className="flex items-baseline justify-between mb-2">
            <h3 className="font-serif italic font-medium text-[22px]">MRR · last 30 days</h3>
            <span className="font-sans text-[10.5px] uppercase tracking-[0.18em] text-pencil">— · today</span>
          </div>
          <hr className="hairline" />
          <div className="flex items-end gap-1.5 h-[140px] mt-5 border-b border-rule">
            {series.map((v, i) => {
              const peak = i === series.length - 2;
              return (
                <span
                  key={i}
                  className={peak ? "bg-tomato" : "bg-ink/85"}
                  style={{ flex: 1, height: `${(v / max) * 100}%` }}
                />
              );
            })}
          </div>
          <div className="flex justify-between mt-2 font-sans text-[9.5px] uppercase tracking-[0.2em] text-pencil">
            <span>30d ago</span><span>15d</span><span>now</span>
          </div>
        </section>

        <section>
          <h3 className="font-serif italic font-medium text-[22px] mb-2">Platform fee</h3>
          <hr className="hairline" />
          <div className="font-serif text-[36px] leading-none mt-4">—</div>
          <div className="font-serif italic text-ink-soft text-[14px]">today · 15% of revenue</div>
          <p className="mt-4 font-serif text-[14px] text-ink-soft leading-[1.55]">
            Platform fee accrues in real time as Stripe webhooks settle. Payouts are weekly to the platform Stripe account.
          </p>
        </section>
      </div>

      <section className="mt-10">
        <div className="flex items-baseline justify-between mb-2">
          <h3 className="font-serif italic font-medium text-[22px]">By creator</h3>
          <span className="font-sans text-[10.5px] uppercase tracking-[0.18em] text-pencil">top 5 today</span>
        </div>
        <hr className="hairline" />
        <p className="mt-3 font-serif italic text-pencil text-[14px]">
          Per-creator revenue requires the billing-aggregation server function — TODO in <code className="font-mono text-[12px] bg-paper-2 px-1 rounded-[2px]">src/server/admin.ts</code>.
        </p>
      </section>
    </AdminShell>
  );
}
