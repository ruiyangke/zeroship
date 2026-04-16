// node:fs/promises stub — throws on actual use.

function notImpl(name: string): (...args: any[]) => Promise<never> {
  return async () => { throw new Error(`fs/promises.${name} is not available in zeroship runtime`); };
}

export const readFile = notImpl("readFile");
export const writeFile = notImpl("writeFile");
export const stat = notImpl("stat");
export const readdir = notImpl("readdir");
export const mkdir = notImpl("mkdir");
export const rm = notImpl("rm");
export const unlink = notImpl("unlink");
export const rename = notImpl("rename");
export const copyFile = notImpl("copyFile");
export const access = notImpl("access");
export const open = notImpl("open");

export default { readFile, writeFile, stat, readdir, mkdir, rm, unlink, rename, copyFile, access, open };
