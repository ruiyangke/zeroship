# Hono Demo

Third-party-framework drop-in — Hono runs unchanged on zeroship because
its default export matches the `{fetch(req, env, ctx)}` contract.

## Run locally

```bash
npm install
npm run dev            # vite-plugin injects bootstrap + starts the worker
```

Then:

```bash
curl http://localhost:3000/
curl http://localhost:3000/hello/world
curl -X POST http://localhost:3000/echo -d '{"ping":"pong"}' -H 'content-type: application/json'
```

## Deploy

```bash
zeroship deploy . --app=<uuid> --control=http://localhost:9090 --key=<master-key>
```
