/**
 * Notes App — full CRUD example using @zeroship/db
 *
 * Deploy:
 *   zeroship deploy examples/notes-app --app=<uuid> --control=http://localhost:9090 --key=<key>
 */

import { createDb } from "@zeroship/db";

const db = createDb({
  notes: {
    title:    { type: String, required: true, min: 1, max: 200 },
    body:     { type: String },
    category: { type: String, default: "general" },
    views:    { type: Number, default: 0 },
    tags:     { type: [String] },
  },
});

export async function createNote(title: string, body: string, category?: string) {
  return db.notes.create({ title, body, ...(category && { category }) });
}

export async function getNotes() {
  return db.notes.find({}).sort({ createdAt: -1 }).limit(100);
}

export async function getNote(id: string) {
  return db.notes.findOne({ _id: id });
}

export async function updateNote(id: string, changes: Record<string, unknown>) {
  return db.notes.updateOne({ _id: id }, { $set: changes });
}

export async function deleteNote(id: string) {
  return db.notes.deleteOne({ _id: id });
}

export async function searchNotes(query: string) {
  return db.notes.find({ title: { $ilike: `%${query}%` } }).limit(50);
}

export async function getNotesByCategory(category: string) {
  return db.notes.find({ category }).sort({ createdAt: -1 });
}

export async function getCategories() {
  return db.notes.distinct("category");
}

export async function countNotes() {
  return db.notes.countDocuments({});
}

export async function incViews(id: string) {
  return db.notes.updateOne({ _id: id }, { $inc: { views: 1 } });
}

export async function getStats() {
  return db.notes.aggregate([
    { $group: { _id: "$category", count: { $sum: 1 }, totalViews: { $sum: "$views" } } },
    { $sort: { count: -1 } },
  ]);
}
