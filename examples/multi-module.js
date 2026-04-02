// Multi-module app — main entry imports from other modules
// Tests: ESM imports, module isolation, shared state

// NOTE: This is the entry module. In a real multi-module app,
// the bundler would have resolved imports. Here we simulate
// what the bundled output looks like after import resolution.

// Simulated "db" module (would be db.js in a real project)
const db = {
    users: [],
    nextId: 1,
    add(name, email) {
        const user = { id: this.nextId++, name, email, createdAt: Date.now() };
        this.users.push(user);
        return user;
    },
    find(id) {
        return this.users.find(u => u.id === id) || null;
    },
    list() {
        return [...this.users];
    },
    remove(id) {
        const idx = this.users.findIndex(u => u.id === id);
        if (idx === -1) return false;
        this.users.splice(idx, 1);
        return true;
    },
};

// Simulated "validators" module
function validateEmail(email) {
    return email && email.includes("@") && email.includes(".");
}

function validateName(name) {
    return name && name.length >= 2 && name.length <= 50;
}

// Exported RPC methods
export function createUser(name, email) {
    if (!validateName(name)) throw new Error("Invalid name: 2-50 characters required");
    if (!validateEmail(email)) throw new Error("Invalid email");
    return db.add(name, email);
}

export function getUser(id) {
    return db.find(id);
}

export function listUsers() {
    return db.list();
}

export function deleteUser(id) {
    return db.remove(id);
}

export function userCount() {
    return db.list().length;
}
