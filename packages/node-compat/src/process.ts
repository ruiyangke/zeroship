// node:process polyfill — uses the runtime's process.env global.

const _process = (globalThis as any).process ?? {};

export const env: Record<string, string | undefined> = _process.env ?? {};
export const version: string = _process.version ?? "v20.0.0";
export const versions: Record<string, string> = { node: "20.0.0" };
export const platform: string = "linux";
export const arch: string = "x64";
export const argv: string[] = [];
export const argv0: string = "zeroship";
export const pid: number = 1;
export const ppid: number = 0;
export const title: string = "zeroship";
export const execPath: string = "/usr/bin/zeroship";

export function exit(_code?: number): never { throw new Error("process.exit() is not supported"); }
export function nextTick(fn: (...args: any[]) => void, ...args: any[]): void { queueMicrotask(() => fn(...args)); }
export function cwd(): string { return "/"; }
export function chdir(_dir: string): void { throw new Error("process.chdir() is not supported"); }
export function uptime(): number { return 0; }
export function hrtime(prev?: [number, number]): [number, number] {
  const now = performance.now();
  const secs = Math.floor(now / 1000);
  const nanos = Math.floor((now % 1000) * 1e6);
  if (prev) return [secs - prev[0], nanos - prev[1]];
  return [secs, nanos];
}
hrtime.bigint = (): bigint => BigInt(Math.floor(performance.now() * 1e6));

export function emitWarning(msg: string): void { console.warn("Warning:", msg); }
export function memoryUsage() { return { rss: 0, heapTotal: 0, heapUsed: 0, external: 0, arrayBuffers: 0 }; }

export const stdout = { write(s: string) { console.log(s); return true; }, isTTY: false };
export const stderr = { write(s: string) { console.error(s); return true; }, isTTY: false };
export const stdin = { read() { return null; }, isTTY: false };

export const release = { name: "zeroship" };

const processObj = {
  env, version, versions, platform, arch, argv, argv0, pid, ppid, title,
  execPath, exit, nextTick, cwd, chdir, uptime, hrtime, emitWarning,
  memoryUsage, stdout, stderr, stdin, release,
};

export default processObj;
