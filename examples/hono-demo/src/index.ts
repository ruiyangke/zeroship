// Hono drop-in demo.
//
// Hono's default export already matches the `{fetch(req, env, ctx)}`
// contract — no adapter needed. Every feature of Hono works out of the
// box: middleware, route groups, context, JSON validation, etc.
//
// This file is what a creator writes; the zeroship platform runs it
// unchanged.

import { Hono } from "hono";
import { logger } from "hono/logger";
import { timing } from "hono/timing";

type Env = {
  // Creator-configured bindings appear here. e.g.
  //   STRIPE_KEY: string;
  // zeroship populates these at deploy time from `zeroship secret set`.
};

const app = new Hono<{ Bindings: Env }>();

app.use("*", logger());
app.use("*", timing());

app.get("/", (c) => c.text("Hono works on zeroship."));

app.get("/hello/:name", (c) => c.json({
  hello: c.req.param("name"),
  at: new Date().toISOString(),
}));

app.post("/echo", async (c) => c.json(await c.req.json()));

app.get("/health", (c) => c.json({ ok: true, runtime: "zeroship" }));

// Any 404 falls through to Hono's default handler — but we override
// it so the error shape matches zeroship's uniform
// {message, name} wire.
app.notFound((c) => c.json({ message: "Not Found", name: "NotFound" }, 404));

app.onError((err, c) =>
  c.json({ message: err.message, name: err.name ?? "Error" }, 500),
);

export default app;
