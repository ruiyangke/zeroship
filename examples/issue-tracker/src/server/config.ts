import { defineApp } from "@zeroship/server";

// RPC auth is fail-closed: a procedure without a resource policy defaults to
// `auth: "user"` behind the gateway. Every procedure is listed explicitly so
// the intended boundary is reviewable and a newly added RPC cannot disappear
// into a build warning.
//
// The ten anonymous procedures are the public Bugzilla-style browsing and
// reporting surface. Every write, account-scoped read, and administrative read
// remains authenticated. Anonymous entries need `publiclyAccessible: true` as
// an explicit acknowledgement that they are intentionally public.
export default defineApp({
  resources: {
    // Issues
    "rpc:issues.create": { auth: "user" },
    "rpc:issues.get": { auth: "anon", publiclyAccessible: true },
    "rpc:issues.update": { auth: "user" },
    "rpc:issues.search": { auth: "anon", publiclyAccessible: true },
    "rpc:issues.changeStatus": { auth: "user" },
    "rpc:issues.resolve": { auth: "user" },
    "rpc:issues.reopen": { auth: "user" },
    "rpc:issues.markDuplicate": { auth: "user" },
    "rpc:issues.reassign": { auth: "user" },
    "rpc:issues.setKind": { auth: "user" },
    "rpc:issues.setSeverity": { auth: "user" },
    "rpc:issues.setPriority": { auth: "user" },
    "rpc:issues.move": { auth: "user" },

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
    "rpc:votes.cast": { auth: "user" },
    "rpc:votes.listMine": { auth: "user" },
    "rpc:watchers.add": { auth: "user" },
    "rpc:watchers.remove": { auth: "user" },
    "rpc:watchers.list": { auth: "user" },
    "rpc:seeAlso.add": { auth: "user" },
    "rpc:seeAlso.remove": { auth: "user" },
    "rpc:seeAlso.list": { auth: "user" },
    "rpc:groups.create": { auth: "user" },
    "rpc:flagTypes.create": { auth: "user" },
    "rpc:flagTypes.list": { auth: "user" },
    "rpc:groups.list": { auth: "user" },
    "rpc:groups.members": { auth: "user" },
    "rpc:groups.addMember": { auth: "user" },
    "rpc:groups.removeMember": { auth: "user" },
    "rpc:products.restrict": { auth: "user" },
    "rpc:products.unrestrict": { auth: "user" },
    "rpc:issues.restrict": { auth: "user" },
    "rpc:issues.unrestrict": { auth: "user" },

    // Product structure.
    //
    // NOT gated on admin, deliberately and contrary to Bugzilla, where these
    // need `editcomponents`. Any authenticated user can create or rename a
    // product here. The reason is that the first account bootstraps as the
    // only admin, so gating these would mean a second user could not set up
    // anything to file issues against -- which makes the example unusable as a
    // demo. `assertCanViewProduct` is NOT a substitute: it returns true for
    // any product with no group restriction, so it gates almost nothing here.
    //
    // If you copy this app for real use, gate these with `requireAdmin`.
    "rpc:products.list": { auth: "anon", publiclyAccessible: true },
    "rpc:products.get": { auth: "user" },
    "rpc:products.resolve": { auth: "anon", publiclyAccessible: true },
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
    "rpc:users.resolve": { auth: "user" },
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
