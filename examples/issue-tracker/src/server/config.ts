import { defineApp } from "@zeroship/server";

// RPC auth is fail-closed: a procedure without a resource policy defaults to
// `auth: "user"` behind the gateway. Every procedure is listed explicitly so
// the intended boundary is reviewable and a newly added RPC cannot disappear
// into a build warning.
//
// The nine anonymous procedures are the public Bugzilla-style browsing and
// reporting surface. Every write, account-scoped read, and administrative read
// remains authenticated. Anonymous entries need `publiclyAccessible: true` as
// an explicit acknowledgement that they are intentionally public.
export default defineApp({
  resources: {
    // Bugs
    "rpc:bugs.create": { auth: "user" },
    "rpc:bugs.get": { auth: "anon", publiclyAccessible: true },
    "rpc:bugs.update": { auth: "user" },
    "rpc:bugs.search": { auth: "anon", publiclyAccessible: true },
    "rpc:bugs.changeStatus": { auth: "user" },
    "rpc:bugs.resolve": { auth: "user" },
    "rpc:bugs.reopen": { auth: "user" },
    "rpc:bugs.markDuplicate": { auth: "user" },
    "rpc:bugs.reassign": { auth: "user" },
    "rpc:bugs.setSeverity": { auth: "user" },
    "rpc:bugs.setPriority": { auth: "user" },
    "rpc:bugs.move": { auth: "user" },

    // Comments
    "rpc:comments.add": { auth: "user" },
    "rpc:comments.list": { auth: "anon", publiclyAccessible: true },
    "rpc:comments.edit": { auth: "user" },
    "rpc:comments.setPrivate": { auth: "user" },

    // Attachments
    "rpc:attachments.upload": { auth: "user" },
    "rpc:attachments.list": { auth: "user" },
    "rpc:attachments.get": { auth: "user" },
    "rpc:attachments.setObsolete": { auth: "user" },
    "rpc:attachments.delete": { auth: "user" },

    // Dependencies and duplicates
    "rpc:deps.add": { auth: "user" },
    "rpc:deps.remove": { auth: "user" },
    "rpc:deps.tree": { auth: "user" },
    "rpc:deps.graph": { auth: "user" },
    "rpc:dupes.list": { auth: "user" },

    // Keywords, flags, and CC
    "rpc:keywords.list": { auth: "user" },
    "rpc:keywords.create": { auth: "user" },
    "rpc:keywords.attach": { auth: "user" },
    "rpc:keywords.detach": { auth: "user" },
    "rpc:flags.set": { auth: "user" },
    "rpc:flags.clear": { auth: "user" },
    "rpc:flags.listRequests": { auth: "user" },
    "rpc:flags.list": { auth: "user" },
    "rpc:cc.add": { auth: "user" },
    "rpc:cc.remove": { auth: "user" },
    "rpc:cc.list": { auth: "user" },
    "rpc:cc.listMine": { auth: "user" },

    // Products and administration
    "rpc:products.list": { auth: "anon", publiclyAccessible: true },
    "rpc:products.get": { auth: "user" },
    "rpc:products.create": { auth: "user" },
    "rpc:products.update": { auth: "user" },
    "rpc:components.list": { auth: "user" },
    "rpc:components.create": { auth: "user" },
    "rpc:components.update": { auth: "user" },
    "rpc:versions.list": { auth: "user" },
    "rpc:versions.create": { auth: "user" },
    "rpc:milestones.list": { auth: "user" },
    "rpc:milestones.create": { auth: "user" },

    // Search and saved searches
    "rpc:search.query": { auth: "user" },
    "rpc:search.quick": { auth: "user" },
    "rpc:savedSearches.list": { auth: "user" },
    "rpc:savedSearches.save": { auth: "user" },
    "rpc:savedSearches.delete": { auth: "user" },

    // Users and notifications
    "rpc:users.me": { auth: "user" },
    "rpc:users.list": { auth: "user" },
    "rpc:users.get": { auth: "user" },
    "rpc:users.updatePrefs": { auth: "user" },
    "rpc:notifications.list": { auth: "user" },
    "rpc:notifications.markRead": { auth: "user" },
    "rpc:notifications.unreadCount": { auth: "user" },

    // Reports
    "rpc:reports.summary": { auth: "anon", publiclyAccessible: true },
    "rpc:reports.byComponent": { auth: "anon", publiclyAccessible: true },
    "rpc:reports.byAssignee": { auth: "anon", publiclyAccessible: true },
    "rpc:reports.trend": { auth: "anon", publiclyAccessible: true },
    "rpc:reports.timeToResolve": { auth: "anon", publiclyAccessible: true },
  },
});
