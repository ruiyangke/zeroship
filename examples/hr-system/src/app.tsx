import { model } from "@zeroship/db"

const employees = model("employees", {
  first_name: { type: String, required: true },
  last_name: { type: String, required: true },
  email: { type: String, required: true },
  department_id: { type: Number },
  status: { type: String, default: "active" },
})

const departments = model("departments", {
  name: { type: String, required: true },
  code: { type: String, required: true },
})

// Server functions — compiler extracts these
export async function getEmployees() {
  return employees.find({ status: "active" }).sort({ last_name: 1 })
}

export async function getDepartments() {
  return departments.find({}).sort({ name: 1 })
}

export async function searchEmployees(query: string) {
  return employees.find({ first_name: { $ilike: `%${query}%` } }).limit(20)
}

// Client component — compiler keeps this, replaces server calls with RPC stubs
export default function App() {
  const [emps, setEmps] = React.useState([])
  const [depts, setDepts] = React.useState([])
  const [search, setSearch] = React.useState("")

  React.useEffect(() => {
    getEmployees().then(r => setEmps(r.data || []))
    getDepartments().then(r => setDepts(r.data || []))
  }, [])

  const handleSearch = async (q) => {
    setSearch(q)
    if (q.length > 0) {
      const { data } = await searchEmployees(q)
      setEmps(data || [])
    } else {
      const { data } = await getEmployees()
      setEmps(data || [])
    }
  }

  return (
    <div style={{ fontFamily: "system-ui", maxWidth: 800, margin: "40px auto", padding: "0 20px" }}>
      <h1>HR Dashboard</h1>

      <div style={{ marginBottom: 20 }}>
        <input
          value={search}
          onChange={e => handleSearch(e.target.value)}
          placeholder="Search employees..."
          style={{ padding: "8px 12px", fontSize: 14, width: "100%", borderRadius: 6, border: "1px solid #ccc" }}
        />
      </div>

      <h2>Employees ({emps.length})</h2>
      <table style={{ width: "100%", borderCollapse: "collapse" }}>
        <thead>
          <tr style={{ borderBottom: "2px solid #333" }}>
            <th style={{ textAlign: "left", padding: 8 }}>Name</th>
            <th style={{ textAlign: "left", padding: 8 }}>Email</th>
            <th style={{ textAlign: "left", padding: 8 }}>Status</th>
          </tr>
        </thead>
        <tbody>
          {emps.map(e => (
            <tr key={e._id} style={{ borderBottom: "1px solid #eee" }}>
              <td style={{ padding: 8 }}>{e.first_name} {e.last_name}</td>
              <td style={{ padding: 8 }}>{e.email}</td>
              <td style={{ padding: 8 }}>{e.status}</td>
            </tr>
          ))}
        </tbody>
      </table>

      <h2>Departments ({depts.length})</h2>
      <ul>
        {depts.map(d => (
          <li key={d._id}>{d.name} ({d.code})</li>
        ))}
      </ul>
    </div>
  )
}
