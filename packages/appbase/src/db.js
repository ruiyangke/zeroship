import Database from 'better-sqlite3'
import { randomUUID } from 'node:crypto'

export function createDb(path = 'appbase.db') {
  const sqlite = new Database(path)
  sqlite.pragma('journal_mode = WAL')
  sqlite.pragma('foreign_keys = ON')

  function collection(name) {
    sqlite.exec(`
      CREATE TABLE IF NOT EXISTS "${name}" (
        id TEXT PRIMARY KEY,
        data JSON NOT NULL,
        created_at TEXT DEFAULT (datetime('now')),
        updated_at TEXT DEFAULT (datetime('now'))
      )
    `)

    return {
      async insert(doc) {
        const id = randomUUID()
        const row = { id, ...doc }
        sqlite.prepare(`INSERT INTO "${name}" (id, data) VALUES (?, ?)`).run(id, JSON.stringify(row))
        return row
      },

      async find(filter) {
        const rows = sqlite.prepare(`SELECT data FROM "${name}"`).all()
        const docs = rows.map(r => JSON.parse(r.data))
        if (!filter) return docs
        return docs.filter(doc =>
          Object.entries(filter).every(([k, v]) => doc[k] === v)
        )
      },

      async delete(id) {
        sqlite.prepare(`DELETE FROM "${name}" WHERE id = ?`).run(id)
      },

      async update(id, updates) {
        const existing = sqlite.prepare(`SELECT data FROM "${name}" WHERE id = ?`).get(id)
        if (!existing) throw new Error(`Document ${id} not found`)
        const doc = { ...JSON.parse(existing.data), ...updates, updated_at: new Date().toISOString() }
        sqlite.prepare(`UPDATE "${name}" SET data = ?, updated_at = datetime('now') WHERE id = ?`).run(JSON.stringify(doc), id)
        return doc
      }
    }
  }

  return {
    collection,
    close() { sqlite.close() }
  }
}
