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
zeroship deploy ./dist/app.zship --app=<id> --control=<url> --token=<PAT>
```
