import { defineApp } from "@zeroship/server";

// This demonstration has no end-user accounts. Objects remain scoped to its app.
export default defineApp({ resources: {
  "rpc:gallery.put": { auth: "anonymous", publiclyAccessible: true },
  "rpc:gallery.get": { auth: "anonymous", publiclyAccessible: true },
  "rpc:gallery.list": { auth: "anonymous", publiclyAccessible: true },
  "rpc:gallery.delete": { auth: "anonymous", publiclyAccessible: true },
  "rpc:gallery.putLarge": { auth: "anonymous", publiclyAccessible: true },
  "rpc:gallery.getLargeHash": { auth: "anonymous", publiclyAccessible: true },
} });
