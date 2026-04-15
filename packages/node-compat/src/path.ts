// node:path polyfill — POSIX only (no Windows support).

export const sep = "/";
export const delimiter = ":";

export function join(...parts: string[]): string {
  return normalize(parts.filter(Boolean).join("/"));
}

export function resolve(...parts: string[]): string {
  let resolved = "";
  for (let i = parts.length - 1; i >= 0; i--) {
    resolved = parts[i] + (resolved ? "/" + resolved : "");
    if (parts[i].startsWith("/")) break;
  }
  return normalize("/" + resolved);
}

export function normalize(p: string): string {
  const isAbs = p.startsWith("/");
  const parts = p.split("/").filter(Boolean);
  const result: string[] = [];
  for (const part of parts) {
    if (part === "..") result.pop();
    else if (part !== ".") result.push(part);
  }
  return (isAbs ? "/" : "") + result.join("/") || ".";
}

export function basename(p: string, ext?: string): string {
  const base = p.split("/").pop() || "";
  if (ext && base.endsWith(ext)) return base.slice(0, -ext.length);
  return base;
}

export function dirname(p: string): string {
  const parts = p.split("/");
  parts.pop();
  return parts.join("/") || ".";
}

export function extname(p: string): string {
  const base = basename(p);
  const dot = base.lastIndexOf(".");
  return dot > 0 ? base.slice(dot) : "";
}

export function isAbsolute(p: string): boolean { return p.startsWith("/"); }

export function relative(from: string, to: string): string {
  const f = resolve(from).split("/").filter(Boolean);
  const t = resolve(to).split("/").filter(Boolean);
  let i = 0;
  while (i < f.length && i < t.length && f[i] === t[i]) i++;
  return [...Array(f.length - i).fill(".."), ...t.slice(i)].join("/") || ".";
}

export function parse(p: string) {
  const dir = dirname(p);
  const base = basename(p);
  const ext = extname(p);
  const name = ext ? base.slice(0, -ext.length) : base;
  return { root: isAbsolute(p) ? "/" : "", dir, base, ext, name };
}

export function format(obj: { dir?: string; root?: string; base?: string; name?: string; ext?: string }): string {
  const dir = obj.dir || obj.root || "";
  const base = obj.base || (obj.name || "") + (obj.ext || "");
  return dir ? dir + "/" + base : base;
}

export const posix = { sep, delimiter, join, resolve, normalize, basename, dirname, extname, isAbsolute, relative, parse, format };
export const win32 = posix; // No Windows support
export default { sep, delimiter, join, resolve, normalize, basename, dirname, extname, isAbsolute, relative, parse, format, posix, win32 };
