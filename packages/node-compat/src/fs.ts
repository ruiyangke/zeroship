// node:fs stub — throws on actual use. No filesystem in the V8 runtime.

function notImpl(name: string): (...args: any[]) => never {
  return () => { throw new Error(`fs.${name} is not available in zeroship runtime`); };
}

export const readFileSync = notImpl("readFileSync");
export const writeFileSync = notImpl("writeFileSync");
export const existsSync = (): boolean => false;
export const mkdtempSync = notImpl("mkdtempSync");
export const mkdirSync = notImpl("mkdirSync");
export const statSync = notImpl("statSync");
export const readdirSync = notImpl("readdirSync");
export const unlinkSync = notImpl("unlinkSync");
export const rmdirSync = notImpl("rmdirSync");
export const renameSync = notImpl("renameSync");
export const copyFileSync = notImpl("copyFileSync");
export const accessSync = notImpl("accessSync");
export const chmodSync = notImpl("chmodSync");
export const constants = { F_OK: 0, R_OK: 4, W_OK: 2, X_OK: 1 };

export default {
  readFileSync, writeFileSync, existsSync, mkdtempSync, mkdirSync,
  statSync, readdirSync, unlinkSync, rmdirSync, renameSync, copyFileSync,
  accessSync, chmodSync, constants,
};
