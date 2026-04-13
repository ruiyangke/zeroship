// Todo App — client-side JS
// Calls the appbase backend via JSON-RPC

const APP_NAME = "todo";

async function rpc(method, params = []) {
  const res = await fetch(`/apps/${APP_NAME}/rpc`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "X-Api-Key": localStorage.getItem("apiKey") || "",
    },
    body: JSON.stringify({ jsonrpc: "2.0", method, params, id: 1 }),
  });
  const json = await res.json();
  return json.result;
}

async function addTodo() {
  const input = document.getElementById("input");
  const text = input.value.trim();
  if (!text) return;
  const result = await rpc("addTodo", [text]);
  if (result.error) {
    alert(result.error.message || "Failed to add");
    return;
  }
  input.value = "";
  refresh();
}

async function toggleTodo(id, done) {
  await rpc("toggleTodo", [id, done]);
  refresh();
}

async function deleteTodo(id) {
  await rpc("deleteTodo", [id]);
  refresh();
}

async function refresh() {
  const result = await rpc("getTodos");
  const todos = result.data || [];
  const list = document.getElementById("list");
  const stats = document.getElementById("stats");

  if (todos.length === 0) {
    list.innerHTML = '<div class="empty">No todos yet. Add one above.</div>';
    stats.textContent = "";
    return;
  }

  list.innerHTML = todos.map(t => `
    <li class="item ${t.done ? 'done' : ''}">
      <button class="check ${t.done ? 'checked' : ''}" onclick="toggleTodo(${t._id}, ${!t.done})">
        ${t.done ? '&#10003;' : ''}
      </button>
      <span class="text">${escapeHtml(t.text)}</span>
      <button class="delete" onclick="deleteTodo(${t._id})">&times;</button>
    </li>
  `).join("");

  const remaining = todos.filter(t => !t.done).length;
  stats.textContent = `${remaining} item${remaining !== 1 ? 's' : ''} left`;
}

function escapeHtml(s) {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

// Handle Enter key
document.getElementById("input").addEventListener("keydown", e => {
  if (e.key === "Enter") addTodo();
});

// Initial load
refresh();
