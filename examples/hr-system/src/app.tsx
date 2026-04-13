import React, { useState, useEffect } from "react";
import { createRoot } from "react-dom/client";
import { model } from "@zeroship/db";

// --- Server-side: models + data access ---

const employees = model("employees", {
  first_name: { type: String, required: true },
  last_name: { type: String, required: true },
  email: { type: String, required: true },
  department_id: { type: Number },
  salary: { type: Number },
  status: { type: String, default: "active" },
  skills: { type: [String] },
});

const departments = model("departments", {
  name: { type: String, required: true },
  code: { type: String, required: true },
  headcount: { type: Number, default: 0 },
});

export async function getEmployees() {
  return employees.find({ status: "active" }).sort({ last_name: 1 });
}

export async function getDepartments() {
  return departments.find({}).sort({ name: 1 });
}

export async function searchEmployees(query: string) {
  return employees.find({ first_name: { $ilike: `%${query}%` } }).limit(20);
}

export async function getDashboardStats() {
  const { data: totalEmps } = await employees.countDocuments({ status: "active" });
  const { data: totalDepts } = await departments.countDocuments({});
  return { data: { totalEmps, totalDepts }, error: null };
}

// --- Client-side: React UI ---

function Dashboard({ stats }: { stats: any }) {
  return (
    <div style={{ display: "flex", gap: 16, marginBottom: 24 }}>
      <StatCard label="Employees" value={stats?.totalEmps ?? "—"} color="#646cff" />
      <StatCard label="Departments" value={stats?.totalDepts ?? "—"} color="#22c55e" />
    </div>
  );
}

function StatCard({ label, value, color }: { label: string; value: any; color: string }) {
  return (
    <div style={{
      flex: 1, padding: 20, borderRadius: 12,
      background: `${color}15`, border: `1px solid ${color}30`,
    }}>
      <div style={{ fontSize: 32, fontWeight: 700, color }}>{value}</div>
      <div style={{ fontSize: 14, color: "#888", marginTop: 4 }}>{label}</div>
    </div>
  );
}

function EmployeeTable({ employees: emps }: { employees: any[] }) {
  if (emps.length === 0) {
    return <p style={{ color: "#666", textAlign: "center", padding: 40 }}>No employees found</p>;
  }
  return (
    <table style={{ width: "100%", borderCollapse: "collapse" }}>
      <thead>
        <tr style={{ borderBottom: "2px solid #333" }}>
          <th style={th}>Name</th>
          <th style={th}>Email</th>
          <th style={th}>Status</th>
        </tr>
      </thead>
      <tbody>
        {emps.map((e: any) => (
          <tr key={e._id} style={{ borderBottom: "1px solid #222" }}>
            <td style={td}>{e.first_name} {e.last_name}</td>
            <td style={td}>{e.email}</td>
            <td style={td}>
              <span style={{
                padding: "2px 8px", borderRadius: 4, fontSize: 12,
                background: e.status === "active" ? "#22c55e20" : "#ef444420",
                color: e.status === "active" ? "#22c55e" : "#ef4444",
              }}>
                {e.status}
              </span>
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function DepartmentList({ departments: depts }: { departments: any[] }) {
  return (
    <div style={{ display: "flex", flexWrap: "wrap", gap: 12 }}>
      {depts.map((d: any) => (
        <div key={d._id} style={{
          padding: "12px 16px", borderRadius: 8,
          background: "#1a1a2e", border: "1px solid #2a2a3e",
        }}>
          <div style={{ fontWeight: 600 }}>{d.name}</div>
          <div style={{ fontSize: 12, color: "#666" }}>{d.code}</div>
        </div>
      ))}
    </div>
  );
}

const th: React.CSSProperties = { textAlign: "left", padding: "8px 12px", fontSize: 13, color: "#888" };
const td: React.CSSProperties = { padding: "10px 12px" };

export default function App() {
  const [emps, setEmps] = useState<any[]>([]);
  const [depts, setDepts] = useState<any[]>([]);
  const [stats, setStats] = useState<any>(null);
  const [search, setSearch] = useState("");

  useEffect(() => {
    getEmployees().then((r: any) => setEmps(r.data || []));
    getDepartments().then((r: any) => setDepts(r.data || []));
    getDashboardStats().then((r: any) => setStats(r.data));
  }, []);

  const handleSearch = async (q: string) => {
    setSearch(q);
    if (q.length > 0) {
      const r = await searchEmployees(q);
      setEmps((r as any).data || []);
    } else {
      const r = await getEmployees();
      setEmps((r as any).data || []);
    }
  };

  return (
    <div style={{
      fontFamily: "-apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif",
      maxWidth: 900, margin: "0 auto", padding: "40px 24px",
      color: "#e0e0e0", background: "#0f0f0f", minHeight: "100vh",
    }}>
      <h1 style={{ fontWeight: 300, letterSpacing: "0.05em", marginBottom: 32 }}>
        HR Dashboard
      </h1>

      <Dashboard stats={stats} />

      <div style={{ marginBottom: 24 }}>
        <input
          value={search}
          onChange={(e) => handleSearch(e.target.value)}
          placeholder="Search employees..."
          style={{
            width: "100%", padding: "10px 14px", fontSize: 14,
            borderRadius: 8, border: "1px solid #2a2a3e",
            background: "#1a1a2e", color: "#e0e0e0", outline: "none",
          }}
        />
      </div>

      <h2 style={{ fontSize: 18, fontWeight: 500, marginBottom: 12 }}>
        Employees ({emps.length})
      </h2>
      <EmployeeTable employees={emps} />

      <h2 style={{ fontSize: 18, fontWeight: 500, margin: "32px 0 12px" }}>
        Departments ({depts.length})
      </h2>
      <DepartmentList departments={depts} />
    </div>
  );
}

// Mount
createRoot(document.getElementById("root")!).render(<App />);
