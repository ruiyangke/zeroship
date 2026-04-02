// Todo API — CRUD with KV storage
// Tests: kv, multiple exports, stateful logic

const PREFIX = "todo:";
let nextId = parseInt(kv.get("todo:nextId") || "1");

function saveNextId() {
    kv.set("todo:nextId", String(nextId));
}

export function add(title) {
    const id = nextId++;
    saveNextId();
    const todo = { id, title, done: false, createdAt: Date.now() };
    kv.set(PREFIX + id, JSON.stringify(todo));
    return todo;
}

export function get(id) {
    const raw = kv.get(PREFIX + id);
    if (!raw) return null;
    return JSON.parse(raw);
}

export function list() {
    const keys = kv.list().filter(k => k.startsWith(PREFIX) && k !== "todo:nextId");
    return keys.map(k => JSON.parse(kv.get(k))).sort((a, b) => a.id - b.id);
}

export function toggle(id) {
    const raw = kv.get(PREFIX + id);
    if (!raw) return null;
    const todo = JSON.parse(raw);
    todo.done = !todo.done;
    kv.set(PREFIX + id, JSON.stringify(todo));
    return todo;
}

export function remove(id) {
    const key = PREFIX + id;
    if (!kv.get(key)) return false;
    kv.delete(key);
    return true;
}

export function clear() {
    const keys = kv.list().filter(k => k.startsWith(PREFIX));
    keys.forEach(k => kv.delete(k));
    nextId = 1;
    saveNextId();
    return { cleared: keys.length };
}
