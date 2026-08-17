import { useEffect, useState, type ReactNode } from "react";
import { Badge } from "../ui/Badge";
import { Banner } from "../ui/Banner";
import { Tabs } from "../ui/Tabs";
import { Button } from "../ui/Button";
import { Input } from "../ui/Input";
import { KindBadge, PriorityBadge, ResolutionBadge, SeverityBadge, StatusBadge } from "../components/Badges";
import { updateIssue } from "../api";
import { invalidatedBy } from "../lib/query-keys";
import { useAppMutation, useIssue, useProduct, useProducts } from "../lib/queries";
import { ErrorState, Loading } from "../components/StateViews";
import { AttachmentsPanel } from "../components/issue-detail/AttachmentsPanel";
import { CcPanel } from "../components/issue-detail/CcPanel";
import { CommentsPanel } from "../components/issue-detail/CommentsPanel";
import { FieldsPanel } from "../components/issue-detail/FieldsPanel";
import { FlagsPanel } from "../components/issue-detail/FlagsPanel";
import { SecurityPanel } from "../components/issue-detail/SecurityPanel";
import { SeeAlsoPanel } from "../components/issue-detail/SeeAlsoPanel";
import { VotesPanel } from "../components/issue-detail/VotesPanel";
import { HistoryPanel } from "../components/issue-detail/HistoryPanel";
import { DependenciesPanel, DuplicatesPanel } from "../components/issue-detail/RelationsPanel";
import { KeywordsPanel } from "../components/issue-detail/KeywordsPanel";
import { errorMessage } from "../components/rpc";
import { isVisitor, useSession } from "../components/session";
import { RailFieldset } from "../components/issue-detail/RailFieldset";
import { RailSection } from "../components/issue-detail/RailSection";
import { FieldError, Page } from "../components/AppPrimitives";

type Tab = "details" | "history";

/**
 * Main-column related content that is disabled as one unit for visitors.
 *
 * This is deliberately not a RailFieldset: it owns a responsive panel grid,
 * not the rail's label/value/action tracks. The locator stays on the native
 * fieldset so the signed-out specs keep checking the real disabled boundary.
 */
function IssueDetailExtras({
  children,
  disabled,
}: {
  children: ReactNode;
  disabled: boolean;
}) {
  return (
    <fieldset
      className="issue-detail-extras m-0 mt-7 grid grid-cols-[repeat(auto-fit,minmax(15rem,1fr))] items-start gap-x-8 gap-y-7 border-0 border-t border-line p-0 pt-5 disabled:opacity-50 [&>section]:m-0 [&>section]:min-w-0"
      disabled={disabled}
    >
      {children}
    </fieldset>
  );
}

/**
 * Editing the issue's title, where the title is.
 *
 * This used to be "Edit summary" at the top of the metadata rail: a control
 * several hundred pixels from the words it changes, in the column reserved for
 * facts ABOUT the issue. A title is not metadata about itself, and an action
 * belongs beside its object.
 */
function TitleEditor({
  issue,
  onDone,
}: {
  issue: { id: string; summary: string };
  /** Closes the editor. It no longer reports WHETHER anything changed: the
   *  mutation invalidates the issue itself, so there is nothing for the caller
   *  to do differently on a save than on an escape. */
  onDone: () => void;
}) {
  const [draft, setDraft] = useState(issue.summary);

  const rename = useAppMutation(
    (summary: string) => updateIssue({ id: issue.id, changes: { summary } }),
    () => invalidatedBy.issueChanged(issue.id),
  );
  const busy = rename.isPending;

  const save = () => {
    const summary = draft.trim();
    if (!summary) return;
    rename.mutate(summary, { onSuccess: onDone });
  };

  return (
    <div className="mb-2 flex max-w-[46ch] flex-col gap-2">
      <Input
        aria-label="Summary"
        value={draft}
        disabled={busy}
        onChange={(event) => setDraft(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            save();
          }
          // Escape abandons, and the draft dies with the component.
          if (event.key === "Escape") onDone();
        }}
      />
      <div className="flex min-w-0 flex-row flex-wrap items-center justify-start gap-2">
        <Button variant="filled" disabled={busy || !draft.trim()} onClick={save}>
          Save
        </Button>
        <Button variant="plain" disabled={busy} onClick={onDone}>
          Cancel
        </Button>
      </div>
      {rename.error ? <FieldError>{errorMessage(rename.error)}</FieldError> : null}
    </div>
  );
}

export function IssueDetailPage({
  id,
  commentNumber,
}: {
  id: string;
  /** From #/issues/<id>/c/<n> -- scroll to that comment once it exists. */
  commentNumber?: number;
}) {
  const issueQ = useIssue(id);
  const [tab, setTab] = useState<Tab>("details");

  // Land on the comment a permalink names.
  //
  // Waits for the element rather than scrolling on mount: the thread arrives
  // over RPC, so at mount there is nothing to scroll to and a naive
  // scrollIntoView would silently do nothing -- which is what a permalink
  // that "works" but goes nowhere looks like.
  useEffect(() => {
    if (!commentNumber) return;
    let cancelled = false;
    let tries = 0;
    const find = () => {
      if (cancelled) return;
      const el = document.getElementById(`comment-${commentNumber}`);
      if (el) {
        // scrollIntoView walks up to the nearest scrollable ancestor, which
        // here is the shell's main rather than the document.
        el.scrollIntoView({ block: "center" });
        el.classList.add("is-linked", "rounded", "bg-accent-soft");
        return;
      }
      if (tries++ < 40) window.setTimeout(find, 100);
    };
    find();
    return () => {
      cancelled = true;
    };
  }, [commentNumber, id]);
  // CONTROLLED, because <details open> is not. Anything inside the fold that
  // reloads the issue -- voting, setting a flag, restricting it -- re-renders
  // this subtree, and an uncontrolled element comes back closed. The fold
  // would snap shut under someone in the middle of using it, which is both a
  // real defect and why the specs saw the panel become "not stable, then not
  // visible" while clicking.
  const [moreOpen, setMoreOpen] = useState(false);
  const [editingTitle, setEditingTitle] = useState(false);
  const productsQ = useProducts();
  // Issues are public, so unlike the dashboard this page stays READABLE without
  // an identity -- what it must not do is offer controls that cannot work.
  const session = useSession();
  const signedOut = isVisitor(session);

  const productId = issueQ.data?.product?.id ?? null;
  // The issue's product structure -- versions, milestones, components, flag
  // types -- read by key rather than fetched into state by an effect. The
  // effect this replaces had to cancel itself against a stale response and
  // clear the previous product's detail by hand; a key does both by being a
  // different key.
  const productQ = useProduct(productId);
  const productDetail = productQ.data ?? null;

  if (issueQ.isPending) return <Loading label="Loading issue..." />;
  if (issueQ.isError) {
    return <ErrorState error={issueQ.error} onRetry={() => void issueQ.refetch()} />;
  }

  const detail = issueQ.data;
  if (!detail.product || !detail.component) {
    return (
      <ErrorState
        error={new Error("This issue references a product or component that no longer resolves.")}
      />
    );
  }
  const product = detail.product;
  const component = detail.component;

  // Everything the log might name by id. Built from what the page already
  // has, so the history tab costs no extra request: the issue's own product and
  // component, the people issues.get resolved, and the product structure the
  // fields panel loaded.
  const historyLabels: Record<string, string> = {
    [product.id]: product.name,
    [component.id]: component.name,
    ...Object.fromEntries(
      Object.values(detail.people ?? {}).map((person) => [person.id, person.name ?? person.handle]),
    ),
    ...Object.fromEntries((productDetail?.versions ?? []).map((v) => [v.id, v.name])),
    ...Object.fromEntries((productDetail?.milestones ?? []).map((m) => [m.id, m.name])),
    ...Object.fromEntries((productDetail?.components ?? []).map((c) => [c.id, c.name])),
    // Issues named by the log: dependsOn / blocks / duplicateOfId store another
    // issue's typed id as the value, so without these the history reads "set
    // Depends On to issue_0346W0Ole6amXDN9RKvzW6".
    ...Object.fromEntries(
      Object.entries(detail.issueRefs ?? {}).map(([id, ref]) => [id, ref.label]),
    ),
  };

  return (
    <Page>
      {/* The key, the title and the state, in that reading order.
          These used to share one line: a mono key, a long summary and four
          badges all at the same height, so a title that ran long squeezed the
          badges and the eye had no obvious place to start. The key is now a
          quiet line above, the summary owns its own line at heading size, and
          the state sits under it where it reads as a caption about the issue
          rather than as more title. */}
      <div className="mb-1 block">
        <span className="mr-2 mb-1 inline-block font-mono text-sm tracking-wide text-ink-muted">
          {detail.product ? `${detail.product.key}-${detail.issue.number}` : detail.issue.id}
        </span>
        {editingTitle ? (
          <TitleEditor issue={detail.issue} onDone={() => setEditingTitle(false)} />
        ) : (
          <div className="group/title flex min-w-0 flex-row flex-wrap items-center justify-start gap-2">
            <h1 className="m-0 max-w-[46ch] text-xl leading-tight tracking-tight">
              {detail.issue.summary}
            </h1>
            {signedOut ? null : (
              <Button
                variant="plain"
                className="opacity-0 transition-opacity duration-fast group-hover/title:opacity-100 group-focus-within/title:opacity-100 [@media(hover:none)]:opacity-100"
                aria-label="Edit summary"
                onClick={() => setEditingTitle(true)}
              >
                Edit
              </Button>
            )}
          </div>
        )}
        <div className="mt-0 flex min-w-0 flex-row flex-wrap items-center justify-start gap-2">
          <StatusBadge status={detail.issue.status} />
          <ResolutionBadge resolution={detail.issue.resolution ?? null} />
          {/* Before severity, because it qualifies it: "blocker" answers how
              bad, and only kind answers how bad AT WHAT. Without this the top
              of a feature request and the top of a crash report were the same
              three badges. */}
          <KindBadge kind={detail.issue.kind} />
          <SeverityBadge severity={detail.issue.severity} />
          <PriorityBadge priority={detail.issue.priority} />
          <span className="text-base text-ink-muted">
            {detail.product ? detail.product.name : null}
            {detail.component ? ` / ${detail.component.name}` : null}
          </span>
        </div>
      </div>

      {/* Said once, at the top, rather than discovered one 401 at a time.
          A signed-out visitor could read this page and still be offered a
          comment box, an Edit button on every comment and three editable
          selects -- every one of which fails on use. The issue stays readable
          because issues are public; the controls that cannot work go away. */}
      {signedOut ? (
        <Banner intent="info" title="You are not signed in">
          This issue is public, so you can read it. Sign in to comment or change any
          of its fields.
        </Banner>
      ) : null}
      <Tabs
        value={tab}
        onValueChange={(value) => {
          if (value === "details" || value === "history") setTab(value);
        }}
        lazyMount
      >
        <Tabs.List>
          <Tabs.Tab value="details">Details</Tabs.Tab>
          <Tabs.Tab value="history">
            <span>History</span>
            <Badge intent="neutral" variant="soft">
              {detail.activities.length}
            </Badge>
          </Tabs.Tab>
          <Tabs.Indicator />
        </Tabs.List>

        <Tabs.Panel value="details">
          <div className="grid grid-cols-[minmax(0,1fr)_22rem] items-start gap-0 max-wide:grid-cols-1">
          {/* The conversation IS the issue. It used to sit under a screen of
              editable fields -- summary, status, severity, priority,
              assignee, product, component, version, whiteboard, OS, platform,
              URL -- so the description, which is what the issue actually says,
              started below the fold. Fields are metadata and metadata goes in
              the rail. */}
          <div className="issue-detail-main flex min-w-0 flex-col gap-5 pr-10 max-wide:pr-0">
            {/* No `attachments` / `onAttachmentsChanged` props: the thread and
                the roll-up below now ask `useAttachments(issueId)` for
                themselves. Same key, so it is still ONE request -- what the
                page owning it used to buy, minus the callback each side had to
                remember. */}
            <CommentsPanel
              issueId={id}
              readOnly={signedOut}
              activities={detail.activities}
              people={detail.people}
              labels={historyLabels}
            />
            {/* Everything below is about the issue WITHOUT being metadata about
                it: files, links to other issues, who is watching, what is
                pending review. They lived in the rail, where measurement put
                them at 1856px stacked inside a 352px column -- a column whose
                whole job is to stay glanceable. Here they get the main
                column's width and pair up two-across instead of forming one
                long ribbon. */}
            {/* Same disabling wrapper as the rail, for the same reason: these
                panels each carry an Add or Upload form that answers 401
                without a session. A fieldset covers all nine and any tenth,
                where nine readOnly props would each be a thing to remember.
                Their CONTENT still renders -- attachments, watchers and
                linked issues are readable facts about a public issue. */}
            <IssueDetailExtras disabled={signedOut}>
              <AttachmentsPanel issueId={id} />
            </IssueDetailExtras>

            {/* The long tail, folded.
                Six of these nine panels report ABSENCE on a typical issue -- no
                duplicates, no linked reports, no keywords, no flags, no votes,
                no group -- and each one drew a full card to say so. That is
                four hundred pixels of grid carrying no information, directly
                under the conversation. Files, CC and dependencies stay out
                because they are the ones that usually have something in them;
                the rest are one click away, and the summary says so rather
                than making you open it to find out. */}
            {/* A button and a conditional, NOT <details>.
                React manages the open attribute while the browser also owns
                it, and the two disagreed: any re-render inside the fold reset
                the element, so a panel would go "not stable, then not
                visible" under a click. One owner of the state removes the
                argument entirely. */}
            <div className="issue-detail-more mt-3">
              <Button
                variant="plain"
                className="max-w-full whitespace-normal text-start"
                aria-expanded={moreOpen}
                onClick={() => setMoreOpen((open) => !open)}
              >
                {moreOpen ? "Hide" : "Show"} flags, votes and security
              </Button>
              {moreOpen ? (
              <IssueDetailExtras disabled={signedOut}>
              {/* No `activities` prop: the panel reads real flags from
                  flags.list instead of replaying the issue's history. */}
              {/* No `onChanged` props: each panel's mutation declares
                  `issueChanged` itself. That invalidates the issue, its list
                  and every report total, including when a flag or vote changes
                  status as a side effect. */}
              <FlagsPanel
                issueId={id}
                flagTypes={productDetail?.flagTypes ?? null}
              />
              <VotesPanel
                issueId={id}
                voteCount={detail.issue.voteCount}
                maxVotesPerIssue={productDetail?.product.maxVotesPerIssue ?? 0}
                votingEnabled={(productDetail?.product.votesPerUser ?? 0) > 0}
              />
              <SecurityPanel issueId={id} />
              </IssueDetailExtras>
              ) : null}
            </div>
          </div>
          {/* The rail is a sticky column only where the grid above it is two
              columns, which is `wide` (1101px) and up. Below that it is a
              block stacked under the conversation, so it wants none of the
              sticky geometry and a top border instead of a left one.

              Both halves are stated as the width that WANTS them. Saying it
              the other way -- turn sticky on at `desktop` and off again under
              `wide` -- needs `!` on every undo, because Tailwind emits
              `(width>=901px)` after `not all and (width>=1101px)` and so the
              on-rules win the 901-1100 overlap on source order alone. */}
          <div className="issue-detail-side flex min-w-0 flex-col gap-5 border-line [&_*]:max-w-full [&>*]:shrink-0 wide:sticky wide:top-0 wide:max-h-[calc(100dvh-4rem)] wide:self-start wide:overflow-y-auto wide:overscroll-contain wide:border-l wide:pt-1 wide:pb-4 wide:pl-7 max-wide:mt-5 max-wide:border-t max-wide:pt-5">
            <FieldsPanel
              issue={detail.issue}
              people={detail.people}
              product={product}
              component={component}
              productDetail={productDetail}
              products={productsQ.data?.map((p) => ({ id: p.id, name: p.name })) ?? []}
              // No `fetchProductDetail` / `onUpdated` seams: the move control
              // reads its target through `useProduct`, and every field write
              // invalidates `issueChanged` at the mutation that made it stale.
              readOnly={signedOut}
            />
            {/* State and relations, not narrative -- so their position must
                not depend on how long the conversation is. Below the composer
                they sank further down the page with every reply; an issue with
                forty comments put "who is CC'd" a thousand pixels from the
                top. Here they are anchored.

                Each one is a single line until asked to open (RailDisclosure),
                which is what makes this survivable: the last time these lived
                in the rail they rendered list AND form at all times and the
                stack measured 1856px in a 352px column. */}
            {/* Labelled like every other rail group, so the space above it
                reads as a heading's space rather than as a gap nobody meant.
                These four are about OTHER things -- people and issues -- which
                is a different kind of fact from severity or component. */}
            <RailSection title="Links" />
            <RailFieldset disabled={signedOut} grouped>
              {/* No `onChanged`: attaching a keyword is a relation change, and
                  the panel's own mutation drops the issue detail that
                  `activities` -- and so the attached set it renders -- comes
                  from. */}
              <KeywordsPanel issueId={id} activities={detail.activities} />
              <CcPanel issueId={id} />
              <DependenciesPanel issueId={id} />
              <DuplicatesPanel
                issueId={id}
                duplicateOfId={detail.issue.duplicateOfId ?? null}
                labels={historyLabels}
              />
              <SeeAlsoPanel issueId={id} />
            </RailFieldset>
          </div>
          </div>
        </Tabs.Panel>
        <Tabs.Panel value="history">
          <HistoryPanel
            activities={detail.activities}
            labels={historyLabels}
            people={detail.people}
          />
        </Tabs.Panel>
      </Tabs>
    </Page>
  );
}
