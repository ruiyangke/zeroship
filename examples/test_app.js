// This is what the compiler would output for the Rust runtime target
// No imports — `db` is injected as a global by runtime.js

const todos = db.collection('todos')

async function addTodo(text) {
  return await todos.insert({ text, done: false })
}

async function getTodos() {
  return await todos.find()
}

async function toggleTodo(id, done) {
  return await todos.update(id, { done })
}

async function deleteTodo(id) {
  return await todos.delete(id)
}

// Register for JSON-RPC dispatch
globalThis.__rpc = { addTodo, getTodos, toggleTodo, deleteTodo }

console.log('Server functions registered: ' + Object.keys(globalThis.__rpc).join(', '))
