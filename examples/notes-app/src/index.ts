/**
 * Notes App — full CRUD example using @appbase/db
 *
 * Deploy:
 *   appbase deploy examples/notes-app --app=<uuid> --control=http://localhost:9090 --key=<key>
 *
 * API (JSON-RPC):
 *   createNote(title, body, category?)   → { _id, title, body, category, views, createdAt }
 *   getNotes()                           → [{ _id, title, body, ... }, ...]
 *   getNote(id)                          → { _id, title, ... } | null
 *   updateNote(id, changes)              → { matchedCount, modifiedCount }
 *   deleteNote(id)                       → { deletedCount }
 *   searchNotes(query)                   → [{ _id, title, body, ... }]
 *   getNotesByCategory(category)         → [{ _id, title, ... }]
 *   getCategories()                      → ["tech", "food", ...]
 *   countNotes()                         → number
 *   incViews(id)                         → { matchedCount, modifiedCount }
 *   getStats()                           → [{ category, count, totalViews }]
 */

import { model } from "@appbase/db";

// --- Model definition (Mongoose style) ---

const Notes = model("notes", {
  title:    { type: String, required: true, min: 1, max: 200 },
  body:     { type: String },
  category: { type: String, default: "general" },
  views:    { type: Number, default: 0 },
  tags:     { type: [String] },
});

// --- CRUD exports ---

export async function createNote(title: string, body: string, category?: string) {
  return Notes.create({
    title,
    body,
    ...(category && { category }),
  });
}

export async function getNotes() {
  return Notes.find({}).sort({ createdAt: -1 }).limit(100);
}

export async function getNote(id: string) {
  return Notes.findOne({ _id: id });
}

export async function updateNote(id: string, changes: Record<string, unknown>) {
  return Notes.updateOne({ _id: id }, { $set: changes });
}

export async function deleteNote(id: string) {
  return Notes.deleteOne({ _id: id });
}

// --- Search & filter ---

export async function searchNotes(query: string) {
  return Notes.find({ title: { $ilike: `%${query}%` } }).limit(50);
}

export async function getNotesByCategory(category: string) {
  return Notes.find({ category }).sort({ createdAt: -1 });
}

export async function getCategories() {
  return Notes.distinct("category");
}

// --- Counts & stats ---

export async function countNotes() {
  return Notes.countDocuments({});
}

export async function incViews(id: string) {
  return Notes.updateOne({ _id: id }, { $inc: { views: 1 } });
}

export async function getStats() {
  return Notes.aggregate([
    { $group: { _id: "$category", count: { $sum: 1 }, totalViews: { $sum: "$views" } } },
    { $sort: { count: -1 } },
  ]);
}
