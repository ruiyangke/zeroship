/**
 * `@zeroship/auth/react` — React adapter.
 *
 * STUB for Slice 2b. The headless client (`@zeroship/auth/client`) is the
 * stable, framework-neutral surface; this entry will grow `AuthProvider`,
 * `useAuth`, `SignInButton`, and `SignIn` in 2b. Exported now (empty) so the
 * `./react` subpath resolves and the package's export map is complete from 2a.
 *
 * Re-exports the public client surface so consumers can `import { createAuthClient }
 * from "@zeroship/auth/react"` ergonomically once the provider lands.
 */

export { createAuthClient, type AuthClient } from "./client";
export type {
  AuthChangeEvent,
  AuthClientOptions,
  Session,
  SignInOptions,
  SignOutOptions,
  User,
} from "./types";
