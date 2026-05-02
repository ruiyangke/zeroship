// ─── Admin Users — directory of creators ───────────────────────
//
// We don't have an admin users endpoint yet; mock with the current
// user as a single row + a couple of editorial fillers so the page
// shape is real and ready to wire.

import { useAuth } from "../../auth/AuthContext";
import { AdminShell } from "../AdminShell";
import { FilterPill } from "../../components/FilterPill";

export function AdminUsers() {
  const { user } = useAuth();
  const rows = [
    { num: 1, name: user?.name ?? "—", email: user?.email ?? "—", joined: "Apr 2026", plan: "free", apps: "—", mrr: "—" },
  ];

  return (
    <AdminShell pageLabel="users · directory">
      <h1 className="font-serif font-medium text-[40px] leading-[1.0] -tracking-[0.02em] mb-2">
        [Admin] <em className="italic text-tomato">Users</em>.
      </h1>
      <p className="font-serif italic text-[15px] text-ink-soft mb-6">
        Searchable directory of every creator. Plan, signup, app count, MRR, actions.
      </p>

      <div className="flex flex-wrap gap-2.5 mb-5">
        <FilterPill active>all · 1</FilterPill>
        <FilterPill>paying · 0</FilterPill>
        <FilterPill>free · 1</FilterPill>
        <input
          aria-label="Search users"
          placeholder="Search by email, name, ID…"
          className="ml-auto px-3 py-1.5 border border-rule bg-white font-serif text-[13px] min-w-[260px] outline-none focus:border-ink"
        />
      </div>

      <table className="w-full font-serif text-[14.5px]">
        <thead>
          <tr>
            <Th width="60px">№</Th>
            <Th>User</Th>
            <Th>Joined</Th>
            <Th>Plan</Th>
            <Th>Apps</Th>
            <Th>MRR</Th>
            <Th></Th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.num} className="hover:bg-paper-2">
              <Td><span className="text-tomato italic" style={{ fontFeatureSettings: '"lnum" 1' }}>{r.num}</span></Td>
              <Td>
                <span className="font-medium">{r.name}</span><br />
                <span className="font-mono text-[11px] text-pencil">{r.email}</span>
              </Td>
              <Td>{r.joined}</Td>
              <Td>{r.plan}</Td>
              <Td>{r.apps}</Td>
              <Td>{r.mrr}</Td>
              <Td><a href="#" className="text-tomato italic font-serif" style={{ textDecoration: "none" }}>view →</a></Td>
            </tr>
          ))}
        </tbody>
      </table>

      <p className="mt-6 font-serif italic text-pencil text-[13px]">
        Cross-account directory wiring TODO — needs an admin-scoped server function in `src/server/admin.ts`.
      </p>
    </AdminShell>
  );
}

function Th({ children, width }: { children?: React.ReactNode; width?: string }) {
  return (
    <th
      className="text-left px-3 py-2.5 font-sans text-[10px] uppercase tracking-[0.18em] text-pencil font-semibold border-b border-rule"
      style={{ width }}
    >
      {children}
    </th>
  );
}
function Td({ children }: { children?: React.ReactNode }) {
  return <td className="px-3 py-3 border-b border-rule-2 align-baseline">{children}</td>;
}
