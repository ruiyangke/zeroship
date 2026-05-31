/**
 * Session-presence breadcrumb (`zs.<host>.is.authenticated`, gateway §4.3).
 *
 * The gateway WRITES this non-HttpOnly cookie server-side on every
 * `/__zeroship/auth/session` (POST exchange + GET `?mint=1`) success and clears it on
 * `/__zeroship/auth/signout` — it is the source of truth. The SDK only READS it (to
 * decide whether to skip a repeat network probe) and writes/clears it as a
 * fast-path mirror. It is an OPTIMIZATION ONLY, never a security boundary: the
 * HttpOnly anchor is authoritative, so a forged or cleared breadcrumb can at
 * most add/remove one cheap network call.
 *
 * The cookie name is keyed on the app HOST exactly as the gateway emits it
 * (`crates/gateway/src/anchors.rs::breadcrumb_cookie_name` → `zs.{host}.is.authenticated`).
 */

import type { CookieJar } from "./env";

/** Derive the breadcrumb cookie name from an app origin (`https://host`). */
export function breadcrumbName(appOrigin: string): string {
  let host = appOrigin;
  try {
    host = new URL(appOrigin).host;
  } catch {
    // already a bare host
    host = appOrigin.replace(/^https?:\/\//, "").replace(/\/.*$/, "");
  }
  return `zs.${host}.is.authenticated`;
}

export class Breadcrumb {
  private readonly name: string;
  private readonly secure: boolean;

  constructor(
    private readonly cookies: CookieJar,
    appOrigin: string,
  ) {
    this.name = breadcrumbName(appOrigin);
    this.secure = appOrigin.startsWith("https://");
  }

  /** True when the breadcrumb cookie is present. */
  isPresent(): boolean {
    const jar = this.cookies.get();
    if (!jar) return false;
    const prefix = `${this.name}=`;
    return jar
      .split(";")
      .map((c) => c.trim())
      .some((c) => c.startsWith(prefix) && c.slice(prefix.length) === "true");
  }

  /** Fast-path mirror write (the gateway's server write is authoritative). */
  set(): void {
    const secure = this.secure ? "; Secure" : "";
    // Max-Age matches the anchor (30 days = 2_592_000 s).
    this.cookies.set(`${this.name}=true; Path=/; SameSite=Lax${secure}; Max-Age=2592000`);
  }

  /** Clear the breadcrumb (signOut / a definitive 401 login_required). */
  clear(): void {
    const secure = this.secure ? "; Secure" : "";
    this.cookies.set(`${this.name}=; Path=/; SameSite=Lax${secure}; Max-Age=0`);
  }
}
