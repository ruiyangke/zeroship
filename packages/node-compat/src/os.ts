// node:os polyfill — minimal stubs.

export const EOL = "\n";
export function platform(): string { return "linux"; }
export function arch(): string { return "x64"; }
export function tmpdir(): string { return "/tmp"; }
export function homedir(): string { return "/"; }
export function hostname(): string { return "zeroship"; }
export function cpus(): any[] { return []; }
export function totalmem(): number { return 0; }
export function freemem(): number { return 0; }
export function type(): string { return "Linux"; }
export function release(): string { return "0.0.0"; }
export function networkInterfaces(): Record<string, any[]> { return {}; }
export function uptime(): number { return 0; }
export function loadavg(): number[] { return [0, 0, 0]; }
export function userInfo(): any { return { username: "zeroship", uid: 0, gid: 0, shell: "/bin/sh", homedir: "/" }; }
export function endianness(): "BE" | "LE" { return "LE"; }
export const constants = { signals: {}, errno: {}, priority: {} };
export const devNull = "/dev/null";

export default {
  EOL, platform, arch, tmpdir, homedir, hostname, cpus, totalmem, freemem,
  type, release, networkInterfaces, uptime, loadavg, userInfo, endianness,
  constants, devNull,
};
