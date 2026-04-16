// node:url polyfill — re-exports global URL + Node.js-specific helpers.

const _URL = globalThis.URL;
const _URLSearchParams = globalThis.URLSearchParams;
export { _URL as URL, _URLSearchParams as URLSearchParams };

export function parse(urlStr: string): any {
  try {
    const u = new URL(urlStr);
    return { protocol: u.protocol, hostname: u.hostname, port: u.port, pathname: u.pathname, search: u.search, hash: u.hash, href: u.href, host: u.host };
  } catch { return null; }
}

export function format(urlObj: any): string { return String(urlObj); }
export function resolve(from: string, to: string): string { return new URL(to, from).href; }
export function fileURLToPath(url: string | URL): string { return String(url).replace("file://", ""); }
export function pathToFileURL(path: string): URL { return new URL("file://" + path); }
export function domainToASCII(domain: string): string { return domain; }
export function domainToUnicode(domain: string): string { return domain; }

export default { URL, URLSearchParams, parse, format, resolve, fileURLToPath, pathToFileURL, domainToASCII, domainToUnicode };
