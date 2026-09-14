# auth-probe

The fixture for the **auth leg of the dev-vs-deployed seam comparison**
(`tests/e2e_dev_vs_deployed_auth.sh`, scenario 6 of `docs/pilot/e2e-scenarios.md`).
Sibling of `examples/storage-probe` and `examples/workflow-probe`.

It is not a demo. It exists so one identical sequence of auth operations can run
against `pnpm dev` and against the same `.zship` deployed behind the gateway, and
the RESULTS can be diffed.

## What makes it a probe rather than a demo

- **No backend primitive.** No `env.db`, `env.kv` or `env.storage` - only
  `env.auth`. `examples/auth-notes` stores notes in KV, so comparing it deployed
  would compare an auth tier *and* a KV backend at once and neither divergence
  could be attributed. Any divergence this app reports is an auth divergence.
- **One procedure per auth posture.** Anonymous, explicit `auth: "user"`, the
  fail-closed default (declared nowhere), a raw `requireUser()` that is reachable
  anonymously, and an app-level `getUser()` gate. See `src/server/config.ts`.
- **Every procedure returns the RAW `env.auth.getUser()` value**, unmapped.
  `getUser()` is a bare `JSON.parse` of the `ZeroShip-User` payload, so the raw
  object IS the kernel contract surface - key names, key order, and which keys
  are present at all. Mapping it through `@zeroship/auth`'s camelCase `User` type
  would launder the field-level divergences the comparison exists to find.
- **Two dev users differing in ONE variable.** `alpha` has an avatar URL, `beta`
  has `null`. That pair is what turns the `avatar` finding into a measurement.

## `probe.defaulted` is deliberately absent from `src/server/config.ts`

Do not "tidy" it in. A procedure with no declared posture resolves to
`auth: "user"` under the SEC-5 fail-closed default; that is the posture every
procedure of `examples/kv-dashboard` and `examples/auth-uploads-kv` shipped with,
and the one whose dev and deployed answers differ (#163). Declaring it deletes
the measurement rather than fixing anything, and the harness asserts its absence
in the BUILT manifest so the deletion cannot happen silently.

## Running it

```bash
pnpm install --filter ./examples/auth-probe...
pnpm --filter ./examples/auth-probe build     # -> dist/app.zship
./tests/e2e_dev_vs_deployed_auth.sh           # both sides + the diff
```

The dev users and the `AUTH_PROBE_API_PORT` default live in `vite.config.ts`.
The harness re-derives them from that file on every run, so the dev config and
the claims it mints for the deployed session cannot drift apart silently.

The sign-in passwords are not in that file: the dev tier derives one per user
from the id (`devPasswordFor` in `sdks/vite-plugin/src/dev-auth.ts` -- `"dev-"`
plus the first 8 characters after `pws_`), so alpha signs in with
`dev-probealp` and beta with `dev-probebet`. The harness derives them the same
way, and fails if a `password:` field reappears in the config.
