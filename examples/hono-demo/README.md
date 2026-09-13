# Hono Demo

Third-party-framework drop-in — Hono runs unchanged on zeroship because
its default export matches the `{fetch(req, env, ctx)}` contract.

## Run locally

```bash
pnpm install
pnpm dev              # vite-plugin injects bootstrap + starts the worker
```

Then:

```bash
curl http://localhost:3000/
curl http://localhost:3000/hello/world
curl -X POST http://localhost:3000/echo -d '{"ping":"pong"}' -H 'content-type: application/json'
```

## Deploy

```bash
pnpm build            # vite build -> dist/app.zship
zeroship deploy ./dist/app.zship --app=<app-id> --control=http://localhost:9090 --token=<token>
```

`deploy` uploads a `.zship` archive, so it needs the build step first and a path
to the archive rather than to this directory. The token comes from `--token`,
`ZEROSHIP_TOKEN`, or credentials saved by `zeroship login`; there is no
`--key` flag.
