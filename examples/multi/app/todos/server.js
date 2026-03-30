import { db } from 'appbase'

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
