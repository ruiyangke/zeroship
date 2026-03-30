import { db, serve } from 'appbase'

const todos = db.collection('todos')

export async function addTodo(text) {
  return todos.insert({ text, done: false })
}

export async function getTodos() {
  return todos.find()
}

export async function toggleTodo(id, done) {
  return todos.update(id, { done })
}

export async function deleteTodo(id) {
  return todos.delete(id)
}

function App() {
  const [items, setItems] = React.useState([])
  const [input, setInput] = React.useState('')

  const refresh = () => getTodos().then(setItems)

  React.useEffect(() => { refresh() }, [])

  const handleAdd = async (e) => {
    e.preventDefault()
    if (!input.trim()) return
    await addTodo(input.trim())
    setInput('')
    refresh()
  }

  const handleToggle = async (item) => {
    await toggleTodo(item.id, !item.done)
    refresh()
  }

  const handleDelete = async (id) => {
    await deleteTodo(id)
    refresh()
  }

  const remaining = items.filter(t => !t.done).length

  return (
    <div style={styles.wrapper}>
      <div style={styles.container}>
        <header style={styles.header}>
          <div style={styles.logo}>&#9671;</div>
          <h1 style={styles.title}>appbase</h1>
          <p style={styles.subtitle}>single-file full-stack demo</p>
        </header>

        <form onSubmit={handleAdd} style={styles.form}>
          <input
            value={input}
            onChange={e => setInput(e.target.value)}
            placeholder="What needs to be done?"
            style={styles.input}
            autoFocus
          />
          <button type="submit" style={styles.addBtn}>
            Add
          </button>
        </form>

        <ul style={styles.list}>
          {items.map(t => (
            <li key={t.id} style={styles.item}>
              <button
                onClick={() => handleToggle(t)}
                style={{
                  ...styles.check,
                  ...(t.done ? styles.checkDone : {})
                }}
              >
                {t.done ? '✓' : ''}
              </button>
              <span style={{
                ...styles.text,
                ...(t.done ? styles.textDone : {})
              }}>
                {t.text}
              </span>
              <button
                onClick={() => handleDelete(t.id)}
                style={styles.deleteBtn}
              >
                &times;
              </button>
            </li>
          ))}
        </ul>

        {items.length > 0 && (
          <footer style={styles.footer}>
            <span>{remaining} item{remaining !== 1 ? 's' : ''} left</span>
          </footer>
        )}

        {items.length === 0 && (
          <div style={styles.empty}>
            <p style={styles.emptyIcon}>&#9745;</p>
            <p style={styles.emptyText}>No todos yet. Add one above.</p>
          </div>
        )}
      </div>
    </div>
  )
}

const styles = {
  wrapper: {
    minHeight: '100vh',
    background: 'linear-gradient(135deg, #0f0f0f 0%, #1a1a2e 50%, #16213e 100%)',
    display: 'flex',
    alignItems: 'flex-start',
    justifyContent: 'center',
    padding: '80px 20px',
    fontFamily: '-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif',
    color: '#e0e0e0',
  },
  container: {
    width: '100%',
    maxWidth: '480px',
  },
  header: {
    textAlign: 'center',
    marginBottom: '40px',
  },
  logo: {
    fontSize: '32px',
    color: '#646cff',
    marginBottom: '8px',
    opacity: 0.9,
  },
  title: {
    fontSize: '28px',
    fontWeight: 300,
    letterSpacing: '0.15em',
    margin: '0 0 4px 0',
    color: '#ffffff',
  },
  subtitle: {
    fontSize: '13px',
    color: '#666',
    margin: 0,
    letterSpacing: '0.05em',
  },
  form: {
    display: 'flex',
    gap: '8px',
    marginBottom: '24px',
  },
  input: {
    flex: 1,
    padding: '14px 16px',
    fontSize: '15px',
    border: '1px solid #2a2a3e',
    borderRadius: '10px',
    background: 'rgba(255,255,255,0.04)',
    color: '#e0e0e0',
    outline: 'none',
    transition: 'border-color 0.2s',
  },
  addBtn: {
    padding: '14px 24px',
    fontSize: '14px',
    fontWeight: 500,
    border: 'none',
    borderRadius: '10px',
    background: '#646cff',
    color: '#fff',
    cursor: 'pointer',
    transition: 'background 0.2s',
    letterSpacing: '0.03em',
  },
  list: {
    listStyle: 'none',
    margin: 0,
    padding: 0,
    display: 'flex',
    flexDirection: 'column',
    gap: '4px',
  },
  item: {
    display: 'flex',
    alignItems: 'center',
    gap: '12px',
    padding: '12px 16px',
    borderRadius: '10px',
    background: 'rgba(255,255,255,0.03)',
    transition: 'background 0.15s',
  },
  check: {
    width: '22px',
    height: '22px',
    borderRadius: '6px',
    border: '1.5px solid #3a3a5c',
    background: 'transparent',
    cursor: 'pointer',
    display: 'flex',
    alignItems: 'center',
    justifyContent: 'center',
    fontSize: '12px',
    color: 'transparent',
    transition: 'all 0.15s',
    padding: 0,
    flexShrink: 0,
  },
  checkDone: {
    background: '#646cff',
    borderColor: '#646cff',
    color: '#fff',
  },
  text: {
    flex: 1,
    fontSize: '15px',
    transition: 'all 0.15s',
  },
  textDone: {
    textDecoration: 'line-through',
    opacity: 0.4,
  },
  deleteBtn: {
    background: 'none',
    border: 'none',
    color: '#555',
    fontSize: '18px',
    cursor: 'pointer',
    padding: '0 4px',
    opacity: 0.5,
    transition: 'opacity 0.15s',
  },
  footer: {
    padding: '16px 0 0',
    fontSize: '13px',
    color: '#555',
    textAlign: 'center',
  },
  empty: {
    textAlign: 'center',
    padding: '40px 0',
  },
  emptyIcon: {
    fontSize: '32px',
    opacity: 0.2,
    margin: '0 0 8px',
  },
  emptyText: {
    fontSize: '14px',
    color: '#444',
    margin: 0,
  },
}

serve(App)
