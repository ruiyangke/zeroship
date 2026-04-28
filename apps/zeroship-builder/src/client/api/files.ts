// Re-export server-function file API for the FilesTab.

export {
  listFiles as listProjectFiles,
  readFile as readProjectFile,
  writeFile as writeProjectFile,
  deleteFile as deleteProjectFile,
  type FileEntry,
} from "../../server/sandbox";

/** Pick a CodeMirror language by file extension. */
export function languageFor(path: string): "javascript" | "html" | "css" | "json" | "plain" {
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  if (["ts", "tsx", "js", "jsx", "mjs", "cjs"].includes(ext)) return "javascript";
  if (ext === "html") return "html";
  if (ext === "css") return "css";
  if (ext === "json") return "json";
  return "plain";
}
