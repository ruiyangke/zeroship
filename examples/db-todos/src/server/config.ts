import { defineApp } from "@zeroship/server";

// The demo uses a shared public ledger and has no login. Its public procedures
// need explicit anonymous policies for the deployed gateway as well as local
// development. The acceptance suite exercises these policies through real app
// requests. Diagnostic procedures omitted here retain the authenticated default.
export default defineApp({
  resources: {
    "rpc:todos.list": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.listPage": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.get": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.count": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.listWithUser": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.subscribe": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.create": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.setDone": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.archive": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.delete": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.shareToWebhook": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txCommit": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txRollback": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txNested": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txIsolation": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txDepth": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.countTitle": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txParallel": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txOverlap": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txPlainWrite": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txBranchWrites": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txOrphanedWrite": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txRaceStep": { auth: "anonymous", publiclyAccessible: true },
    "rpc:users.seed": { auth: "anonymous", publiclyAccessible: true },
    "rpc:users.getPair": { auth: "anonymous", publiclyAccessible: true },
    "rpc:users.public": { auth: "anonymous", publiclyAccessible: true },
  },
});
