export default function Todos() {
  const [items, setItems] = React.useState([])
  const [input, setInput] = React.useState('')

  const refresh = async () => {
    const res = await fetch('/rpc', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', method: 'getTodos', params: [], id: Date.now() })
    })
    const data = await res.json()
    if (data.result) setItems(data.result)
  }

  React.useEffect(() => { refresh() }, [])

  const handleAdd = async (e) => {
    e.preventDefault()
    if (!input.trim()) return
    await fetch('/rpc', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', method: 'addTodo', params: [input.trim()], id: Date.now() })
    })
    setInput('')
    refresh()
  }

  const handleToggle = async (item) => {
    await fetch('/rpc', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', method: 'toggleTodo', params: [item.id, !item.done], id: Date.now() })
    })
    refresh()
  }

  const handleDelete = async (id) => {
    await fetch('/rpc', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', method: 'deleteTodo', params: [id], id: Date.now() })
    })
    refresh()
  }

  return (
    <div>
      <h1 style={{ fontSize: '24px', fontWeight: 300, marginBottom: '24px' }}>Todos</h1>
      <form onSubmit={handleAdd} style={{ display: 'flex', gap: '8px', marginBottom: '24px' }}>
        <input
          value={input}
          onChange={e => setInput(e.target.value)}
          placeholder="What needs to be done?"
          style={{
            flex: 1, padding: '12px 16px', fontSize: '14px',
            border: '1px solid #2a2a3e', borderRadius: '8px',
            background: 'rgba(255,255,255,0.04)', color: '#e0e0e0', outline: 'none'
          }}
        />
        <button type="submit" style={{
          padding: '12px 20px', fontSize: '14px', border: 'none', borderRadius: '8px',
          background: '#646cff', color: '#fff', cursor: 'pointer'
        }}>Add</button>
      </form>
      <ul style={{ listStyle: 'none', padding: 0, display: 'flex', flexDirection: 'column', gap: '4px' }}>
        {items.map(t => (
          <li key={t.id} style={{
            display: 'flex', alignItems: 'center', gap: '12px',
            padding: '12px 16px', borderRadius: '8px', background: 'rgba(255,255,255,0.03)'
          }}>
            <button onClick={() => handleToggle(t)} style={{
              width: '22px', height: '22px', borderRadius: '6px',
              border: t.done ? 'none' : '1.5px solid #3a3a5c',
              background: t.done ? '#646cff' : 'transparent',
              color: t.done ? '#fff' : 'transparent',
              cursor: 'pointer', fontSize: '12px', display: 'flex',
              alignItems: 'center', justifyContent: 'center', padding: 0, flexShrink: 0
            }}>{t.done ? '\u2713' : ''}</button>
            <span style={{
              flex: 1, fontSize: '14px',
              textDecoration: t.done ? 'line-through' : 'none',
              opacity: t.done ? 0.4 : 1
            }}>{t.text}</span>
            <button onClick={() => handleDelete(t.id)} style={{
              background: 'none', border: 'none', color: '#555',
              fontSize: '18px', cursor: 'pointer', padding: '0 4px'
            }}>&times;</button>
          </li>
        ))}
      </ul>
      {items.length === 0 && (
        <p style={{ textAlign: 'center', color: '#444', padding: '32px 0' }}>No todos yet</p>
      )}
    </div>
  )
}
