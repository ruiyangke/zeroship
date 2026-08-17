# zeroship starter

A minimal React + RPC zeroship app. It keeps messages in server memory so the first build has no database setup.

## Quickstart

```bash
pnpm install
pnpm dev
```

Open the Vite URL and try the message board.

## Build

```bash
pnpm build
```

The zeroship Vite plugin writes the deploy artifact to `dist/app.zship`.

## Deploy

```bash
zeroship login
zeroship deploy
```

The artifact path, the app and the control plane come from `zeroship.jsonc`
(see [`docs/reference/project-config.md`](../../docs/reference/project-config.md)).
That file ships without an `app` key, so the first deploy creates an app named
after the project's `name` and appends its id to the file.
`--app=<id>`, `--control=<url>`, a positional `.zship` path and `--token=<token>`
all still work as overrides.
