export type MaybePromise<T> = T | Promise<T>;
export type ValueProvider<T> = T | (() => MaybePromise<T>);

/** A canonical creator app id. */
export type AppId = `app_${string}`;
/** A canonical platform user id: `usr_` followed by the fixed-width lowercase base36 UUIDv7 body. */
export type UserId = `usr_${string}`;

const APP_ID_PATTERN = /^app_([0-9a-z]{25})$/;

/** Check the complete runtime shape and numeric range of an AppId. */
export function isAppId(value: string): value is AppId {
  const body = APP_ID_PATTERN.exec(value)?.[1];
  if (body === undefined) return false;

  let decoded = 0n;
  for (const character of body) {
    const digit = Number.parseInt(character, 36);
    decoded = decoded * 36n + BigInt(digit);
    if (decoded > (1n << 128n) - 1n) return false;
  }
  return true;
}

export interface ControlClientOptions {
  /** Control-plane origin, for example `http://localhost:9090`. */
  baseUrl: string | URL;
  /** Fetch implementation. Defaults to `globalThis.fetch`. */
  fetch?: typeof fetch;
  /** Master key / bearer token provider. Values without a scheme become `Bearer <value>`. */
  auth?: ValueProvider<string | null | undefined>;
  /** Cookie header provider for server-side session forwarding. */
  cookie?: ValueProvider<string | null | undefined>;
  /** Global headers applied to every request. */
  headers?: ValueProvider<HeadersInit | null | undefined>;
  /** Called for every upstream `Set-Cookie` header. Useful for SSR/RPC proxies. */
  onSetCookie?: (cookie: string, response: Response) => MaybePromise<void>;
}

export interface ControlRequestOptions {
  method?: string;
  query?: Record<string, string | number | boolean | null | undefined>;
  headers?: HeadersInit;
  body?: unknown;
  /** `null` means do not set a content-type. Objects default to JSON. */
  contentType?: string | null;
  parseAs?: "json" | "text" | "void";
}

export interface ControlErrorBody {
  error?: unknown;
  message?: unknown;
  code?: unknown;
  /**
   * Server-minted correlation id. Only responses produced by
   * `infrastructure_error_response` carry this id. Failures such as
   * `control.env.listVars` can remain id-less. The helper's body is generic
   * by design (`{"error":"internal error"}`), and it logs the real cause
   * under the same key and the same value.
   */
  trace_id?: unknown;
  [key: string]: unknown;
}

export class ControlError extends Error {
  readonly status: number;
  readonly statusText: string;
  readonly body: unknown;
  readonly code?: string;
  /**
   * Correlation id lifted off the body. When present, quote it to an
   * operator: the producing helper logged the real cause under the same
   * key. `undefined` when the server did not send one -- never invented
   * here.
   *
   * Named `trace_id` to match the spelling `@zeroship/rpc` already lifts
   * (`packages/rpc/src/error.ts`) rather than adding another id concept to a
   * system that has several that do not join.
   */
  readonly trace_id?: string;
  readonly response: Response;

  constructor(response: Response, body: unknown) {
    super(errorMessage(response, body));
    this.name = "ControlError";
    this.status = response.status;
    this.statusText = response.statusText;
    this.body = body;
    this.response = response;
    const code = isRecord(body) ? body.code : undefined;
    this.code = typeof code === "string" ? code : undefined;
    const traceId = isRecord(body) ? body.trace_id : undefined;
    this.trace_id = typeof traceId === "string" ? traceId : undefined;
  }
}

export interface AppRecord {
  id: AppId;
  name: string;
  plan_id: string;
  deploy_hash: string | null;
  /** Non-null while the app is out of service; null while it is active. */
  archived_at: string | null;
  created_at: string;
  updated_at: string;
}

export interface CreateAppInput {
  name: string;
  plan_id?: string;
}

export interface DeployAppResult {
  deploy_hash: string;
  blobs_uploaded?: number;
  blobs_deduped?: number;
}

export interface SetPlanInput {
  plan_id: string;
}

export interface SetPlanResult {
  updated: boolean;
}

export type UsageCounters = Record<string, number>;

export interface EnvVar {
  key: string;
  value: string;
}

export interface ListVarsResult {
  vars: EnvVar[];
}

export interface ListSecretsResult {
  secrets: string[];
}

export interface ListExposeResult {
  expose: string[];
}

export interface SetExposeInput {
  keys: string[];
}

/**
 * What a rule does when it matches. There is no default: a rule that does not
 * say which one it is, is refused.
 */
export type EgressVerdict = "accept" | "reject";

/**
 * Which of the two things a destination is. The server infers it from the
 * destination's own grammar rather than taking it from you: a value that parses
 * as a CIDR range is `"cidr"`, anything else must parse as an exact DNS name
 * and is `"name"`.
 *
 * The distinction is not cosmetic. A name is decided before the app resolves
 * anything; a range can only be decided against a resolved address.
 */
export type EgressDestinationKind = "name" | "cidr";

/** One egress rule for an app. */
export interface EgressRule {
  app_id: AppId;
  verdict: EgressVerdict;
  kind: EgressDestinationKind;
  /** The exact DNS name, or the range in canonical CIDR form. */
  destination: string;
  port: number;
  created_by: UserId;
  created_at: string;
  note: string | null;
  /**
   * The verdict that actually applies once the whole rule set is read.
   * Rules are an unordered set and any matching reject wins, so an accept
   * range wholly inside a reject range at the same port reports
   * `"reject"` here while `verdict` still reads `"accept"`.
   *
   * Only statically decidable overlaps are reflected. Whether a name lands
   * inside a rejected range depends on what that name resolves to at connect
   * time, which this endpoint does not look up, so a name rule always reports
   * its own verdict.
   */
  effective_verdict: EgressVerdict;
}

/** A `net.requests` hint from the app's manifest. Inert until a rule exists. */
export interface EgressRequest {
  host: string;
  port: number;
  reason: string;
}

/** The plan ceiling on egress. A creator cannot raise any of these. */
export interface EgressRuleLimits {
  /** Bounds accept rules only. A reject rule can only narrow, so it is never charged here. */
  max_accept_rules: number;
  used_accept_rules: number;
  /**
   * Reject rules are capped separately, and for an unrelated reason: every
   * rule rides the projection the runtime polls, so an unbounded reject list
   * is a load problem. It is never a safety bound - a reject can only narrow.
   *
   * Declared because the server has always sent it. Omitting it here meant a
   * creator's rule set could 409 against a ceiling that appeared in no client
   * type and no doc.
   */
  max_reject_rules: number;
  used_reject_rules: number;
  max_sockets: number;
  egress_ceiling_bytes: number;
}

export interface ListEgressRulesResult {
  app_id: AppId;
  rules: EgressRule[];
  requests: EgressRequest[];
  /** Manifest hints with no matching accept rule: what is still refused. */
  pending_requests: EgressRequest[];
  limits: EgressRuleLimits;
}

export interface EgressRuleInput {
  /** Required. Omitting it is a 400 rather than an implied `"accept"`. */
  verdict: EgressVerdict;
  /** An exact DNS name, or an address range in CIDR form. No wildcards. */
  destination: string;
  port: number;
  note?: string;
}

export interface DeleteEgressRuleInput {
  destination: string;
  port: number;
}

export interface SetEgressRuleResult {
  rule: EgressRule;
  /**
   * A one-time explanation, non-null only on an app's FIRST accept rule for
   * an address range. That rule changes how the app refuses destinations it
   * does not allow, and this is the only place that is said. Show it.
   */
  notice: string | null;
}

export interface AuditEntry {
  id: string;
  actor_user_id: UserId | null;
  action: string;
  resource: string | null;
  source_ip: string | null;
  at: string;
}

export interface ListAuditOptions {
  limit?: number;
}

export interface ListAuditResult {
  audit: AuditEntry[];
  count: number;
  limit: number;
}

export interface SetKeyValueInput {
  key: string;
  value: string;
}

export type DeployBody =
  | string
  | ArrayBuffer
  | ArrayBufferView
  | Blob
  | FormData
  | URLSearchParams
  | ReadableStream<Uint8Array>;

export interface DeployOptions {
  contentType?: string;
}

export interface WorkflowSignalTokenInput {
  appId: AppId;
  types: string[];
  ttl: string;
}

export interface WorkflowSignalTokenResult {
  token: string;
  expiresAt: string;
}

export interface WorkflowTopicBroadcastInput {
  appId: AppId;
  type: string;
  payload?: unknown;
  idempotencyKey?: string;
}

export interface WorkflowTopicBroadcastResult {
  id: string;
  topic: string;
}

/**
 * An organization: the root that owns projects, and the party that is billed.
 *
 * `id` is a typed id (`org_...`), never the slug. Every path below takes the
 * id; a slug sent where an id belongs resolves to no membership at all and is
 * refused with nothing useful to read.
 */
export interface OrganizationRecord {
  id: string;
  slug: string;
  name: string;
  billing_email: string;
  /**
   * Set only on a personal organization - the one minted for a creator on
   * their first deploy. No read path branches on it; it is reported so a
   * console can label the row, and so clearing it (which is what transferring
   * ownership does) is visible as the personal-to-shared conversion it is.
   */
  personal_owner_id: UserId | null;
  created_at: string;
  updated_at: string;
  /**
   * When the organization was CLOSED, and `null` while it is live.
   *
   * A closed organization is a historical record. It stays readable, it keeps
   * its members, its invitations and its billing history, and it accepts NO
   * further change - every write against it answers `409 organization
   * dissolved`. It also releases its slug, so the name is free for a new
   * organization to take.
   *
   * `list()` returns closed organizations alongside live ones. A UI that
   * shows a member's organizations should label them rather than hide them:
   * the member is still on the record.
   */
  dissolved_at: string | null;
}

export interface CreateOrganizationInput {
  name: string;
  /** Derived from the name when absent. Globally unique. */
  slug?: string;
  /** Seeded from the caller's own address when absent. */
  billing_email?: string;
}

export interface UpdateOrganizationInput {
  name?: string;
  slug?: string;
  billing_email?: string;
}

export interface ListOrganizationsResult {
  organizations: OrganizationRecord[];
}

/**
 * One seat. `role` is a row in the platform's role ladder, and the two integers
 * beside it are the whole authority model: `rank` orders app and organization
 * authority, `billing_rank` orders money authority, and an actor may act on a
 * target only when `actor.rank > target.rank` AND
 * `actor.billing_rank >= target.billing_rank`.
 *
 * They are reported rather than derived here on purpose. The ladder is data in
 * `zeroship.organization_roles`, so a client that hard-coded the numbers would
 * disagree with the server the day a migration moves one.
 */
export interface OrganizationMemberRecord {
  organization_id: string;
  user_id: UserId;
  email: string;
  name: string;
  role: string;
  rank: number;
  billing_rank: number;
  added_at: string;
}

export interface ListOrganizationMembersResult {
  members: OrganizationMemberRecord[];
}

export interface AddOrganizationMemberInput {
  user_id: UserId;
  role: string;
}

export interface ChangeOrganizationRoleInput {
  role: string;
}

export interface TransferOrganizationOwnershipInput {
  /**
   * The member who becomes owner. They must already hold a seat: transfer
   * re-roles an existing member, it does not admit a new one.
   */
  user_id: UserId;
}

/**
 * A pending or spent invitation. It carries NO token: only the digest is
 * stored, so the secret exists exactly once, in the response to `createInvite`.
 */
export interface InviteRecord {
  id: string;
  organization_id: string;
  email: string;
  role: string;
  issued_at: string;
  expires_at: string;
  consumed_at: string | null;
}

export interface ListInvitesResult {
  invites: InviteRecord[];
}

export interface CreateInviteInput {
  email: string;
  role: string;
}

/**
 * What became of the invitation email.
 *
 * - `sent` - the mailer accepted it and the recipient has the token.
 * - `suppressed` - the address is on the platform's bounce/complaint list, so
 *   nothing was sent. Not an error; deliver the token another way.
 * - `failed` - the transport refused or failed. The invitation still stands
 *   and the token in the same response is the only copy that exists.
 *
 * The invitation row is written and committed BEFORE the send is attempted, so
 * a delivery failure costs an email rather than an invitation.
 */
export type InviteDelivery = "sent" | "suppressed" | "failed";

/**
 * The only response that carries an invitation token.
 *
 * The server stores a digest and nothing else, so this value cannot be read
 * back by any later call: the remedy for a lost one is `revokeInvite` followed
 * by a new `createInvite`.
 *
 * The platform mails the invitation itself, so a client does not have to
 * deliver `token` - but it should show it when `delivery` is not `"sent"`,
 * because in those cases nothing else will. Possessing the token grants
 * nothing on its own: redemption additionally requires the redeeming account's
 * VERIFIED address to be the invited address.
 */
export interface CreatedInvite {
  invite: InviteRecord;
  token: string;
  delivery: InviteDelivery;
}

export interface RedeemInviteInput {
  token: string;
}

export interface ProjectRecord {
  id: string;
  organization_id: string;
  slug: string;
  name: string;
  created_at: string;
}

export interface ListProjectsResult {
  projects: ProjectRecord[];
}

export interface CreateProjectInput {
  name: string;
  /** Derived from the name when absent. Unique within the organization. */
  slug?: string;
}

/** At least one of the two must be present; an empty update is a `400`. */
export interface UpdateProjectInput {
  name?: string;
  slug?: string;
}

/**
 * A project seat. It NARROWS and never widens: a member's authority on a
 * project is `min(organization rank, project rank)`, and a member at admin rank
 * or above reaches every project in the organization with no row here at all.
 *
 * There is no billing dimension, because there is no per-project invoice.
 */
export interface ProjectMemberRecord {
  project_id: string;
  user_id: UserId;
  email: string;
  role: string;
  added_at: string;
}

export interface ListProjectMembersResult {
  members: ProjectMemberRecord[];
}

export interface AddProjectMemberInput {
  user_id: UserId;
  role: string;
}

/**
 * Move an existing project seat to another role.
 *
 * It is one statement, not a removal followed by a grant: a member below admin
 * with no project row reaches nothing, so delete-then-add would blank their
 * access in between, and a failure between the two would leave the seat gone
 * rather than narrowed.
 */
export interface ChangeProjectRoleInput {
  role: string;
}

export class ControlClient {
  readonly #options: ControlClientOptions;
  readonly #fetch: typeof fetch;

  readonly apps = {
    list: (): Promise<AppRecord[]> => this.request("/api/apps"),
    get: (id: AppId): Promise<AppRecord> =>
      this.request(`/api/apps/${pathPart(id)}`),
    create: (input: CreateAppInput): Promise<AppRecord> =>
      this.request("/api/apps", {
        method: "POST",
        body: {
          name: input.name,
          plan_id: input.plan_id ?? "free",
        },
      }),
    archive: (id: AppId): Promise<AppRecord> =>
      this.request(`/api/apps/${pathPart(id)}/archive`, { method: "PUT" }),
    unarchive: (id: AppId): Promise<AppRecord> =>
      this.request(`/api/apps/${pathPart(id)}/archive`, { method: "DELETE" }),
    /**
     * End the app. Terminal, unlike `archive`, and the last step of the
     * account-closure funnel: only after it can the app's project be deleted,
     * then its organization dissolved, then its sole owner erased.
     *
     * `DELETE` on the APP is this; `DELETE` on the app's `archive` resource is
     * the reversible restore above. The app must already be archived, and it
     * returns nothing because there is no record left to hand back that the
     * caller may act on.
     */
    delete: (id: AppId): Promise<void> =>
      this.request(`/api/apps/${pathPart(id)}`, { method: "DELETE" }),
    deploy: (
      id: AppId,
      artifact: DeployBody,
      options: DeployOptions = {},
    ): Promise<DeployAppResult> =>
      this.request(`/api/apps/${pathPart(id)}/deploy`, {
        method: "POST",
        body: artifact,
        contentType: options.contentType ?? "application/x-zship",
      }),
    setPlan: (id: AppId, input: SetPlanInput): Promise<SetPlanResult> =>
      this.request(`/api/apps/${pathPart(id)}/plan`, {
        method: "PUT",
        body: input,
      }),
    usage: (id: AppId): Promise<UsageCounters> =>
      this.request(`/api/apps/${pathPart(id)}/usage`),
    logs: (id: AppId): Promise<string[]> =>
      this.request(`/api/apps/${pathPart(id)}/logs`),
  };

  readonly env = {
    listVars: (appId: AppId): Promise<ListVarsResult> =>
      this.request(`/api/apps/${pathPart(appId)}/vars`),
    setVar: (appId: AppId, input: SetKeyValueInput): Promise<void> =>
      this.request(`/api/apps/${pathPart(appId)}/vars`, {
        method: "POST",
        body: input,
        parseAs: "void",
      }),
    deleteVar: (appId: AppId, key: string): Promise<void> =>
      this.request(`/api/apps/${pathPart(appId)}/vars/${pathPart(key)}`, {
        method: "DELETE",
        parseAs: "void",
      }),
    listSecrets: (appId: AppId): Promise<ListSecretsResult> =>
      this.request(`/api/apps/${pathPart(appId)}/secrets`),
    setSecret: (appId: AppId, input: SetKeyValueInput): Promise<void> =>
      this.request(`/api/apps/${pathPart(appId)}/secrets`, {
        method: "POST",
        body: input,
        parseAs: "void",
      }),
    deleteSecret: (appId: AppId, key: string): Promise<void> =>
      this.request(`/api/apps/${pathPart(appId)}/secrets/${pathPart(key)}`, {
        method: "DELETE",
        parseAs: "void",
      }),
    listExpose: (appId: AppId): Promise<ListExposeResult> =>
      this.request(`/api/apps/${pathPart(appId)}/env/expose`),
    setExpose: (appId: AppId, input: SetExposeInput): Promise<ListExposeResult> =>
      this.request(`/api/apps/${pathPart(appId)}/env/expose`, {
        method: "PUT",
        body: input,
      }),
    listAudit: (
      appId: AppId,
      options: ListAuditOptions = {},
    ): Promise<ListAuditResult> =>
      this.request(`/api/apps/${pathPart(appId)}/audit`, {
        query: { limit: options.limit },
      }),
  };

  /**
   * The app's egress rules. They cover every raw byte stream the app can open
   * - `node:net`, `node:tls` and outbound `WebSocket` - through one rule set.
   * An app with no accept rule opens none of them. `fetch` is the exception
   * and reaches any public host with no rule at all.
   *
   * A rule is a verdict, a destination and a port. The destination is either an
   * exact DNS name (`api.example.com`) or an address range in CIDR form
   * (`93.184.216.0/24`); wildcards such as `*.example.com` are not a grammar
   * this API accepts. Rules are an unordered set, not an ordered list: any
   * matching reject wins, then any matching accept admits, and anything else is
   * refused.
   *
   * Use real public space in a range, not the documentation ranges
   * (`198.51.100.0/24`, `2001:db8::/32`): those are refused by the platform
   * SSRF floor, so such a rule is accepted here and can never admit a connect.
   *
   * Same authority as `env`: `env:read` to list, `env:write` to change.
   */
  readonly egressRules = {
    list: (appId: AppId): Promise<ListEgressRulesResult> =>
      this.request(`/api/apps/${pathPart(appId)}/egress-rules`),
    set: (appId: AppId, input: EgressRuleInput): Promise<SetEgressRuleResult> =>
      this.request(`/api/apps/${pathPart(appId)}/egress-rules`, {
        method: "POST",
        body: input,
      }),
    remove: (appId: AppId, input: DeleteEgressRuleInput): Promise<void> =>
      this.request(`/api/apps/${pathPart(appId)}/egress-rules`, {
        method: "DELETE",
        body: input,
        parseAs: "void",
      }),
  };

  /**
   * Organizations, their members, their invitations and their projects.
   *
   * THE LAYERING IS ONE PATH AND IT IS NOT NEGOTIABLE HERE: an organization
   * owns projects, a project owns apps, and an app reaches its organization
   * only through its project. There is no `organization_id` on an app and no
   * per-app membership; a call that wants "who answers for this app" resolves
   * the app's project, then that project's organization.
   *
   * AUTHORITY IS RESOLVED SERVER-SIDE, PER REQUEST, AND IS NEVER CACHED. Do not
   * cache a member's rank in a client and decide from it: the server re-reads
   * the seat inside the statement that performs each effect, which is what
   * makes a revocation take effect immediately and with no invalidation signal
   * to miss.
   *
   * Every id in these paths is typed: `org_...`, `prj_...`, `ivt_...`, or
   * `usr_...` for `user_id`.
   */
  readonly organizations = {
    /** The organizations the caller holds a seat in. */
    list: (): Promise<ListOrganizationsResult> => this.request("/api/organizations"),
    /** Mint one and seat the caller as its owner. */
    create: (input: CreateOrganizationInput): Promise<OrganizationRecord> =>
      this.request("/api/organizations", { method: "POST", body: input }),
    get: (organizationId: string): Promise<OrganizationRecord> =>
      this.request(`/api/organizations/${pathPart(organizationId)}`),
    /** Rename, re-slug, or change the billed contact. Owner only. */
    update: (
      organizationId: string,
      input: UpdateOrganizationInput,
    ): Promise<OrganizationRecord> =>
      this.request(`/api/organizations/${pathPart(organizationId)}`, {
        method: "PATCH",
        body: input,
      }),

    members: (organizationId: string): Promise<ListOrganizationMembersResult> =>
      this.request(`/api/organizations/${pathPart(organizationId)}/members`),
    addMember: (
      organizationId: string,
      input: AddOrganizationMemberInput,
    ): Promise<OrganizationMemberRecord> =>
      this.request(`/api/organizations/${pathPart(organizationId)}/members`, {
        method: "POST",
        body: input,
      }),
    changeMemberRole: (
      organizationId: string,
      userId: UserId,
      input: ChangeOrganizationRoleInput,
    ): Promise<OrganizationMemberRecord> =>
      this.request(
        `/api/organizations/${pathPart(organizationId)}/members/${pathPart(userId)}`,
        { method: "PATCH", body: input },
      ),
    removeMember: (organizationId: string, userId: UserId): Promise<void> =>
      this.request(
        `/api/organizations/${pathPart(organizationId)}/members/${pathPart(userId)}`,
        { method: "DELETE", parseAs: "void" },
      ),
    /**
     * Give up YOUR OWN seat. It names no user, and there is no parameter that
     * could: the route reaches the bearer's seat by construction.
     *
     * This is the one carve-out in the rank model. Every other membership write
     * needs `actor.rank > target.rank`, and nobody outranks themselves - which
     * is why `removeMember(org, myOwnId)` is refused and this exists. The
     * general inequality is untouched, so it still stops an admin removing a
     * peer.
     *
     * A SOLE OWNER IS REFUSED with `409 last owner`: an organization that keeps
     * no owner is one no route can repair. Call `transferOwnership` first.
     */
    leave: (organizationId: string): Promise<void> =>
      this.request(`/api/organizations/${pathPart(organizationId)}/membership`, {
        method: "DELETE",
        parseAs: "void",
      }),
    /**
     * CLOSE the organization. Owner only, and it cannot be undone.
     *
     * It is a soft close: the row, its members, its invitations and its billing
     * history all survive and stay readable, and every subsequent write answers
     * `409 organization dissolved`. The returned record carries `dissolved_at`.
     *
     * It is REFUSED while the organization still owns projects (`409
     * organization has projects`, carrying the count). Delete them with
     * `projects.delete`, which itself needs each project to own no apps. The
     * order is deliberate: an organization that vanished while apps were still
     * running would leave them with nobody who answers for them.
     *
     * Closing releases the organization's slug, so the name is free for a new
     * organization to take.
     */
    dissolve: (organizationId: string): Promise<OrganizationRecord> =>
      this.request(`/api/organizations/${pathPart(organizationId)}`, {
        method: "DELETE",
      }),
    /**
     * Move the owner seat to another member. It MOVES rather than duplicates:
     * every membership write requires the actor to strictly outrank the target
     * and nothing outranks an owner, so a second owner is not reachable through
     * this API and the caller stops being one.
     */
    transferOwnership: (
      organizationId: string,
      input: TransferOrganizationOwnershipInput,
    ): Promise<void> =>
      this.request(`/api/organizations/${pathPart(organizationId)}/transfer`, {
        method: "POST",
        body: input,
        parseAs: "void",
      }),

    invites: (organizationId: string): Promise<ListInvitesResult> =>
      this.request(`/api/organizations/${pathPart(organizationId)}/invites`),
    /** The one call that returns a token. See {@link CreatedInvite}. */
    createInvite: (
      organizationId: string,
      input: CreateInviteInput,
    ): Promise<CreatedInvite> =>
      this.request(`/api/organizations/${pathPart(organizationId)}/invites`, {
        method: "POST",
        body: input,
      }),
    revokeInvite: (organizationId: string, inviteId: string): Promise<void> =>
      this.request(
        `/api/organizations/${pathPart(organizationId)}/invites/${pathPart(inviteId)}`,
        { method: "DELETE", parseAs: "void" },
      ),
    /**
     * Accept an invitation. Not organization-scoped, because the redeemer holds
     * no seat yet - the token is the capability, and the organization it names
     * comes back in the response.
     *
     * The server re-derives the INVITER's live authority at redemption, so an
     * invitation from someone since demoted is refused even though it was valid
     * when issued.
     */
    redeemInvite: (input: RedeemInviteInput): Promise<OrganizationRecord> =>
      this.request("/api/organization-invites/redeem", {
        method: "POST",
        body: input,
      }),

    /** The projects of this organization the caller can reach. */
    projects: (organizationId: string): Promise<ListProjectsResult> =>
      this.request(`/api/organizations/${pathPart(organizationId)}/projects`),
    createProject: (
      organizationId: string,
      input: CreateProjectInput,
    ): Promise<ProjectRecord> =>
      this.request(`/api/organizations/${pathPart(organizationId)}/projects`, {
        method: "POST",
        body: input,
      }),
  };

  /**
   * A project: what an app actually belongs to, and the only place authority
   * can be narrowed below the organization seat.
   *
   * These are the `/api/projects/*` routes. Creating one is
   * `organizations.createProject`, because a project cannot exist without
   * naming the organization it belongs to.
   */
  readonly projects = {
    get: (projectId: string): Promise<ProjectRecord> =>
      this.request(`/api/projects/${pathPart(projectId)}`),
    /** Rename or re-slug. Admin and above. */
    update: (
      projectId: string,
      input: UpdateProjectInput,
    ): Promise<ProjectRecord> =>
      this.request(`/api/projects/${pathPart(projectId)}`, {
        method: "PATCH",
        body: input,
      }),
    /**
     * Delete a project. Admin and above, and HARD - unlike closing an
     * organization, which is a timestamp. A project names no money record, so
     * nothing has to outlive it, and its `project_members` rows go with it.
     *
     * REFUSED while the project still owns apps (`409 project has apps`,
     * carrying the count). Archive each one with `apps.archive` and then
     * `apps.delete` it. There is no route that moves an app between projects,
     * so that is the whole remedy.
     */
    delete: (projectId: string): Promise<void> =>
      this.request(`/api/projects/${pathPart(projectId)}`, {
        method: "DELETE",
        parseAs: "void",
      }),
    members: (projectId: string): Promise<ListProjectMembersResult> =>
      this.request(`/api/projects/${pathPart(projectId)}/members`),
    addMember: (
      projectId: string,
      input: AddProjectMemberInput,
    ): Promise<ProjectMemberRecord> =>
      this.request(`/api/projects/${pathPart(projectId)}/members`, {
        method: "POST",
        body: input,
      }),
    /** Narrow or widen an existing project seat atomically. */
    changeMemberRole: (
      projectId: string,
      userId: UserId,
      input: ChangeProjectRoleInput,
    ): Promise<ProjectMemberRecord> =>
      this.request(
        `/api/projects/${pathPart(projectId)}/members/${pathPart(userId)}`,
        { method: "PATCH", body: input },
      ),
    removeMember: (projectId: string, userId: UserId): Promise<void> =>
      this.request(
        `/api/projects/${pathPart(projectId)}/members/${pathPart(userId)}`,
        { method: "DELETE", parseAs: "void" },
      ),
  };

  readonly workflows = {
    createSignalToken: (
      runId: string,
      input: WorkflowSignalTokenInput,
    ): Promise<WorkflowSignalTokenResult> =>
      this.request(`/internal/workflows/runs/${pathPart(runId)}/signal-token`, {
        method: "POST",
        headers: { "x-zeroship-app-id": input.appId },
        body: {
          types: input.types,
          ttl: input.ttl,
        },
      }),
    createTopicSignalToken: (
      topic: string,
      input: WorkflowSignalTokenInput,
    ): Promise<WorkflowSignalTokenResult> =>
      this.request(`/internal/workflows/topics/${pathPart(topic)}/signal-token`, {
        method: "POST",
        headers: { "x-zeroship-app-id": input.appId },
        body: {
          types: input.types,
          ttl: input.ttl,
        },
      }),
    publishTopic: (
      topic: string,
      input: WorkflowTopicBroadcastInput,
    ): Promise<WorkflowTopicBroadcastResult> =>
      this.request(`/internal/workflows/topics/${pathPart(topic)}/broadcast`, {
        method: "POST",
        headers: { "x-zeroship-app-id": input.appId },
        body: {
          type: input.type,
          payload: input.payload,
          idempotencyKey: input.idempotencyKey,
        },
      }),
  };

  constructor(options: ControlClientOptions) {
    if (!options.baseUrl) {
      throw new Error("createControlClient requires a baseUrl");
    }
    const fetchImpl = options.fetch ?? globalThis.fetch;
    if (!fetchImpl) {
      throw new Error("createControlClient requires a fetch implementation");
    }
    this.#options = options;
    this.#fetch = fetchImpl;
  }

  async request<T = unknown>(
    path: string,
    options: ControlRequestOptions = {},
  ): Promise<T> {
    const headers = await this.#headers(options);
    const body = encodeBody(options.body, headers, options.contentType);

    const response = await this.#fetch(this.#url(path, options.query), {
      method: options.method ?? "GET",
      headers,
      body,
    });

    await this.#forwardSetCookie(response);

    const parsed = await parseResponseBody(response, options.parseAs);
    if (!response.ok) {
      throw new ControlError(response, parsed);
    }
    return parsed as T;
  }

  async #headers(options: ControlRequestOptions): Promise<Headers> {
    const globalHeaders = await resolveProvider(this.#options.headers);
    const headers = new Headers(globalHeaders ?? undefined);
    const auth = await resolveProvider(this.#options.auth);
    const cookie = await resolveProvider(this.#options.cookie);

    if (auth) {
      headers.set("authorization", authHeader(auth));
    }
    if (cookie) {
      headers.set("cookie", cookie);
    }
    mergeHeaders(headers, options.headers);
    return headers;
  }

  #url(path: string, query?: ControlRequestOptions["query"]): string {
    const base = this.#options.baseUrl.toString();
    const url = new URL(path, base.endsWith("/") ? base : `${base}/`);
    if (query) {
      for (const [key, value] of Object.entries(query)) {
        if (value !== undefined && value !== null) {
          url.searchParams.set(key, String(value));
        }
      }
    }
    return url.toString();
  }

  async #forwardSetCookie(response: Response): Promise<void> {
    if (!this.#options.onSetCookie) return;
    for (const cookie of readSetCookies(response.headers)) {
      await this.#options.onSetCookie(cookie, response);
    }
  }
}

export function createControlClient(options: ControlClientOptions): ControlClient {
  return new ControlClient(options);
}

async function resolveProvider<T>(
  provider: ValueProvider<T> | null | undefined,
): Promise<T | undefined> {
  if (provider === undefined || provider === null) return undefined;
  if (typeof provider === "function") {
    return (provider as () => MaybePromise<T>)();
  }
  return provider;
}

function mergeHeaders(headers: Headers, input: HeadersInit | undefined): void {
  if (!input) return;
  new Headers(input).forEach((value, key) => headers.set(key, value));
}

function authHeader(value: string): string {
  return /^[a-z]+ /i.test(value) ? value : `Bearer ${value}`;
}

function encodeBody(
  body: unknown,
  headers: Headers,
  contentType: string | null | undefined,
): BodyInit | undefined {
  if (body === undefined) return undefined;
  if (shouldJsonEncode(body)) {
    setContentType(headers, contentType ?? "application/json");
    return JSON.stringify(body);
  }
  setContentType(headers, contentType);
  return body as BodyInit;
}

function shouldJsonEncode(body: unknown): body is Record<string, unknown> {
  if (body === null || typeof body !== "object") return false;
  if (body instanceof ArrayBuffer || ArrayBuffer.isView(body)) return false;
  if (typeof Blob !== "undefined" && body instanceof Blob) return false;
  if (typeof FormData !== "undefined" && body instanceof FormData) return false;
  if (typeof URLSearchParams !== "undefined" && body instanceof URLSearchParams) {
    return false;
  }
  if (typeof ReadableStream !== "undefined" && body instanceof ReadableStream) {
    return false;
  }
  return true;
}

function setContentType(
  headers: Headers,
  contentType: string | null | undefined,
): void {
  if (contentType === undefined || contentType === null) return;
  if (!headers.has("content-type")) {
    headers.set("content-type", contentType);
  }
}

async function parseResponseBody(
  response: Response,
  parseAs: ControlRequestOptions["parseAs"],
): Promise<unknown> {
  if (parseAs === "void" || response.status === 204) return undefined;
  if (parseAs === "text") return response.text();
  const contentType = response.headers.get("content-type") ?? "";
  if (parseAs === "json" || contentType.includes("json")) {
    return response.json().catch(() => undefined);
  }
  return response.text();
}

function readSetCookies(headers: Headers): string[] {
  const withGetSetCookie = headers as Headers & {
    getSetCookie?: () => string[];
  };
  const values = withGetSetCookie.getSetCookie?.();
  if (values && values.length > 0) return values;

  const single = headers.get("set-cookie");
  return single ? [single] : [];
}

function errorMessage(response: Response, body: unknown): string {
  if (isRecord(body)) {
    if (typeof body.error === "string") return body.error;
    if (typeof body.message === "string") return body.message;
  }
  if (typeof body === "string" && body.length > 0) return body;
  return `HTTP ${response.status}`;
}

function isRecord(value: unknown): value is ControlErrorBody {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function pathPart(value: string): string {
  return encodeURIComponent(value);
}
