import { db, serve } from 'appbase'

const todos = db.collection('todos')

export async function addTodo(text) {
  return todos.insert({ text, done: false })
}

export async function getTodos() {
  return todos.find()
}

function App() {
  const [items, setItems] = React.useState([])

  React.useEffect(() => {
    getTodos().then(setItems)
  }, [])

  const handleAdd = async () => {
    const text = prompt('Todo text:')
    if (text) {
      await addTodo(text)
      setItems(await getTodos())
    }
  }

  return (
    <div>
      <h1>Todos</h1>
      <button onClick={handleAdd}>Add Todo</button>
      <ul>{items.map(t => <li key={t.id}>{t.text}</li>)}</ul>
    </div>
  )
}

serve(App)
