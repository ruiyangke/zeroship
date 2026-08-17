import { useMemo, useState, type ComponentPropsWithoutRef } from "react";
import { Checkbox } from "../ui/Checkbox";
import { Banner } from "../ui/Banner";
import { Dialog } from "../ui/Dialog";
import { Drawer } from "../ui/Drawer";
import { Tabs } from "../ui/Tabs";
import { Select } from "../ui/Select";
import { Badge } from "../ui/Badge";
import { Button } from "../ui/Button";
import { Field } from "../ui/Field";
import { FilterBar } from "../ui/FilterBar";
import { Input } from "../ui/Input";
import { ListView } from "../ui/ListView";
import { PageHeader } from "../ui/PageHeader";
import { Pagination } from "../ui/Pagination";
import {
  createComponent,
  createMilestone,
  createProduct,
  createVersion,
  updateComponent,
  deleteProduct,
  updateProduct,
} from "../api";
import { invalidatedBy } from "../lib/query-keys";
import {
  useAppMutation,
  useProduct,
  useProducts,
  useReportByComponent,
} from "../lib/queries";
import { FlagTypesAdmin } from "../components/FlagTypesAdmin";
import { GroupsAdmin } from "../components/GroupsAdmin";
import { AsyncSection } from "../components/StateViews";
import { errorMessage } from "../components/rpc";
import { isSignedIn, isVisitor, useSession } from "../components/session";
import type { Product, ProductDetail, ReportByComponent } from "../components/types";
import {
  FieldError,
  FilterControl,
  Hint,
  InlineForm,
  Muted,
  Page,
  SectionHeading,
} from "../components/AppPrimitives";

/** How many products a page of the list holds. */
const PAGE_SIZE = 25;

/**
 * What a product row says about itself beyond its own columns: how many open
 * issues it holds, and which components they are in.
 *
 * Read out of `reports.byComponent`, which is one anonymous call for the whole
 * tracker and is the ONLY bulk source of either number. `components.list` is
 * per-product and `auth: "user"`, so building this from it would be 168
 * authenticated requests and would leave a signed-out visitor with nothing.
 *
 * The consequence, stated because it decides the wording on screen: a
 * component with no open issues is not in this data at all. So the chips are
 * labelled with their counts and read as a BREAKDOWN of the open total beside
 * them -- "Core 8, Parser 4" under "12 open" -- rather than as the product's
 * component list, which they are not and cannot claim to be.
 */
type Rollup = { open: number; components: { name: string; count: number }[] };

function rollupByProduct(rows: ReportByComponent): Map<string, Rollup> {
  const out = new Map<string, Rollup>();
  for (const row of rows) {
    // A row whose component was deleted still counts toward no product we can
    // name, so it is dropped rather than attributed to a guess.
    if (!row.component) continue;
    const entry = out.get(row.component.productId) ?? { open: 0, components: [] };
    entry.open += row.count;
    entry.components.push({ name: row.component.name, count: row.count });
    out.set(row.component.productId, entry);
  }
  for (const entry of out.values()) {
    entry.components.sort((a, b) => b.count - a.count);
  }
  return out;
}

const EMPTY_ROLLUP: Rollup = { open: 0, components: [] };

/** How many component chips fit a row before the rest become "+N more". */
const CHIPS_PER_ROW = 3;

function ProductAdminColumn(props: Omit<ComponentPropsWithoutRef<"div">, "className">) {
  return (
    <div
      {...props}
      className="rounded-lg border border-line bg-surface p-3"
    />
  );
}

function ProductAdminList(props: Omit<ComponentPropsWithoutRef<"ul">, "className">) {
  return (
    <ul
      {...props}
      className="mx-0 mt-0 mb-2 flex list-none flex-col gap-1 p-0 text-md"
    />
  );
}

function ProductAdminListItem(props: Omit<ComponentPropsWithoutRef<"li">, "className">) {
  return <li {...props} className="flex items-center justify-between gap-2" />;
}

function ComponentChips({ rollup }: { rollup: Rollup }) {
  if (rollup.components.length === 0) return null;
  const shown = rollup.components.slice(0, CHIPS_PER_ROW);
  const rest = rollup.components.length - shown.length;
  return (
    <span className="inline-flex max-w-88 flex-wrap items-center justify-end gap-1">
      {shown.map((component) => (
        <Badge key={component.name} intent="neutral" variant="soft">
          {component.name} {component.count}
        </Badge>
      ))}
      {rest > 0 ? <Muted className="text-sm">+{rest} more</Muted> : null}
    </span>
  );
}

function NewProductDialog() {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const [key, setKey] = useState("");
  const [description, setDescription] = useState("");
  const [classification, setClassification] = useState("Unclassified");
  const [allowsUnconfirmed, setAllowsUnconfirmed] = useState(true);

  // A new product moves the list AND the open-issue rollup beside it, which is
  // what `productStructureChanged` names. The dialog no longer takes an
  // `onCreated` prop: it had nothing to say to its parent except "go and
  // refetch", which the invalidation now says to every reader at once.
  const create = useAppMutation(
    (input: Parameters<typeof createProduct>[0]) => createProduct(input),
    () => invalidatedBy.productStructureChanged(),
  );
  const busy = create.isPending;

  const submit = () => {
    if (!name.trim()) return;
    create.mutate(
      {
        name: name.trim(),
        key: key.trim() || undefined,
        description: description || undefined,
        classification,
        allowsUnconfirmed,
      },
      {
        // Emptying the form and closing the dialog stay -- they are what this
        // callback did BESIDES carrying staleness.
        onSuccess: () => {
          setName("");
          setKey("");
          setDescription("");
          setOpen(false);
        },
      },
    );
  };

  return (
    <>
      {/* A plain Button driving the controlled `open`, matching how the issue
          list opens its search builder. */}
      <Button variant="filled" onClick={() => setOpen(true)}>
        New product
      </Button>
      <Dialog open={open} onOpenChange={setOpen}>
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup size="md">
            <Dialog.Header>
              <Dialog.Title>New product</Dialog.Title>
              <Dialog.Description>
                A product is what an issue is filed against. Components, versions and
                milestones are added afterwards.
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Body>
              {/* A real form element with an id, so the submit button can live
                  in the footer where a dialog's primary action belongs and
                  still submit on Enter from any field. */}
              <form
                id="new-product-form"
                onSubmit={(e) => {
                  e.preventDefault();
                  submit();
                }}
              >
                <div className="flex min-w-0 flex-col flex-nowrap items-stretch justify-start gap-3 [&>*]:min-h-0 [&>*]:min-w-0">
                  <Field.Root>
                    <Field.Label>Name</Field.Label>
                    <Input value={name} onChange={(e) => setName(e.target.value)} required />
                  </Field.Root>
                  <Field.Root>
                    <Field.Label>Key</Field.Label>
                    {/* Uppercased as you type rather than rejected afterwards:
                        the server's rule is a leading letter then up to nine
                        more uppercase alphanumerics, and typing "parser" is
                        the obvious way to get it wrong. */}
                    <Input
                      value={key}
                      onChange={(e) => setKey(e.target.value.toUpperCase())}
                      placeholder="PARSER"
                      maxLength={10}
                    />
                    <Field.Description>
                      Half of every issue identifier -- PARSER-12. Derived from the name
                      when left blank, and not changeable afterwards.
                    </Field.Description>
                  </Field.Root>
                  <Field.Root>
                    <Field.Label>Description</Field.Label>
                    <Input
                      value={description}
                      onChange={(e) => setDescription(e.target.value)}
                    />
                  </Field.Root>
                  <Field.Root>
                    <Field.Label>Classification</Field.Label>
                    <Input
                      value={classification}
                      onChange={(e) => setClassification(e.target.value)}
                    />
                  </Field.Root>
                  <Checkbox
                    checked={allowsUnconfirmed}
                    onCheckedChange={(next) => setAllowsUnconfirmed(next === true)}
                    label="Allows UNCONFIRMED"
                  />
                  {create.error ? (
                    <FieldError>{errorMessage(create.error)}</FieldError>
                  ) : null}
                </div>
              </form>
            </Dialog.Body>
            <Dialog.Footer>
              <Dialog.Close className="h-7 px-3 text-base">Cancel</Dialog.Close>
              <Button
                type="submit"
                form="new-product-form"
                variant="filled"
                className="h-7 px-3 text-base"
                disabled={busy || !name.trim()}
              >
                Create product
              </Button>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>
    </>
  );
}

type SortKey = "name" | "open";

/**
 * The browse surface, and the page's reason to exist.
 *
 * It used to open on a create form -- three full-width inputs, a checkbox and a
 * button -- above a 220px column of buttons that could show eight of 168
 * products with no search, beside two thirds of a page reading "Select a
 * product to manage its structure". Almost every visit is "find a product and
 * see what is in it", and that was the one thing the page could not do.
 *
 * So: search first, the list full width and paged, each row carrying what it
 * holds, and creating one is a dialog behind a button in the toolbar.
 */
function ProductBrowser({
  products,
  rollups,
  signedIn,
  refreshing,
}: {
  products: Product[];
  rollups: Map<string, Rollup>;
  signedIn: boolean;
  refreshing: boolean;
}) {
  const [search, setSearch] = useState("");
  const [sort, setSort] = useState<SortKey>("name");
  const [page, setPage] = useState(1);
  const [selected, setSelected] = useState<string | null>(null);

  const matches = useMemo(() => {
    const needle = search.trim().toLowerCase();
    // Name, key AND classification, because all three are on screen and any of
    // them is a reasonable thing to have typed.
    const filtered = needle
      ? products.filter((product) =>
          [product.name, product.key, product.classification]
            .join(" ")
            .toLowerCase()
            .includes(needle),
        )
      : products;
    const sorted = [...filtered];
    if (sort === "open") {
      sorted.sort(
        (left, right) =>
          (rollups.get(right.id)?.open ?? 0) - (rollups.get(left.id)?.open ?? 0) ||
          left.name.localeCompare(right.name),
      );
    }
    return sorted;
  }, [products, rollups, search, sort]);

  // Clamped rather than reset on every keystroke: typing narrows the list, and
  // snapping back to page 1 from page 4 is right, while re-rendering page 1
  // when the result set has not shrunk past the current page is not.
  const pageCount = Math.max(1, Math.ceil(matches.length / PAGE_SIZE));
  const current = Math.min(page, pageCount);
  const visible = matches.slice((current - 1) * PAGE_SIZE, current * PAGE_SIZE);

  const selectedProduct = selected ? products.find((p) => p.id === selected) ?? null : null;

  return (
    <div className="flex flex-col gap-3">
      <FilterBar
        search={search}
        onSearchChange={(next) => {
          setSearch(next);
          setPage(1);
        }}
        // Short enough to READ inside the design system 24rem cap. The longer
        // "Search products by name, key or classification" clipped mid-word
        // once the cap started applying, which looks like a broken field
        // rather than a deliberate width.
        searchPlaceholder="Search products"
        actions={signedIn ? <NewProductDialog /> : null}
      >
        {/* No visible label, matching the search box beside it. The reports
            filters carry visible labels because there are two of them and one
            had one; here the toolbar is a search field and one picker, and a
            "Sort" label over the picker alone would be the same asymmetry the
            other way round. The value states what it is instead. */}
        <FilterControl>
          <Select
            value={sort}
            onValueChange={(next) => setSort((next as SortKey) ?? "name")}
            aria-label="Sort products"
            renderValue={(value) => (value === "open" ? "Sort: open issues" : "Sort: name")}
          >
            <Select.Item value="name">Name</Select.Item>
            <Select.Item value="open">Open issues</Select.Item>
          </Select>
        </FilterControl>
      </FilterBar>

      {/* Says how many of how many, because a filtered list that finds nothing
          and a tracker with no products are different facts. */}
      <Hint flush>
        {matches.length === products.length
          ? `${products.length} ${products.length === 1 ? "product" : "products"}`
          : `${matches.length} of ${products.length} products match`}
      </Hint>

      {matches.length === 0 ? (
        <Hint flush>No product matches that search.</Hint>
      ) : (
        <>
          <ListView
            // Compact: this is a scanning list of 100+ rows, and the roomy
            // default spends a third more page on the same two lines.
            density="compact"
            className={`overflow-hidden rounded-lg border border-line bg-surface${
              refreshing ? " opacity-55 transition-opacity duration-fast" : ""
            }`}
            items={visible.map((product) => {
              const rollup = rollups.get(product.id) ?? EMPTY_ROLLUP;
              return {
                id: product.id,
                title: (
                  <span className="inline-flex flex-wrap items-center gap-2">
                    <span className="font-semibold">{product.name}</span>
                    <Badge intent="neutral" variant="outline">
                      {product.key}
                    </Badge>
                    {product.isActive ? null : (
                      <Badge intent="warning" variant="soft">
                        inactive
                      </Badge>
                    )}
                  </span>
                ),
                description: product.description ?? undefined,
                meta: <ComponentChips rollup={rollup} />,
                trailing: (
                  <Badge
                    intent={rollup.open > 0 ? "info" : "neutral"}
                    variant="soft"
                    className="min-h-7 px-2 text-sm"
                  >
                    {rollup.open} open
                  </Badge>
                ),
                // Signed out the row is plain text. Opening it runs
                // `products.get`, which is `auth: "user"` -- so a clickable row
                // would be an invitation to a panel that can only say
                // "sign-in required". The names themselves are public
                // (`products.list` is anonymous) and stay readable.
                onClick: signedIn ? () => setSelected(product.id) : undefined,
              };
            })}
          />
          {matches.length > PAGE_SIZE ? (
            <Pagination
              page={current}
              pageSize={PAGE_SIZE}
              total={matches.length}
              onPageChange={setPage}
            />
          ) : null}
        </>
      )}

      {/* On demand, not permanently. The editor used to own the right-hand two
          thirds of the page at all times and spent almost all of it reading
          "Select a product to manage its structure" -- the dead panel was the
          default state, and the list was squeezed into the remaining 220px to
          make room for it. */}
      <Drawer open={selected !== null} onOpenChange={(open) => !open && setSelected(null)}>
        <Drawer.Portal>
          <Drawer.Backdrop />
          <Drawer.Content side="end" size="lg">
            <Drawer.Header>
              <Drawer.Title>{selectedProduct?.name ?? "Product"}</Drawer.Title>
              <Drawer.Description>
                Rename it, retire it, and manage its components, versions and milestones.
              </Drawer.Description>
            </Drawer.Header>
            <Drawer.Body>
              {selected ? <ProductEditor productId={selected} /> : null}
            </Drawer.Body>
          </Drawer.Content>
        </Drawer.Portal>
      </Drawer>
    </div>
  );
}

function ProductEditor({ productId }: { productId: string }) {
  const productQ = useProduct(productId);
  return (
    <AsyncSection query={productQ} loadingLabel="Loading product...">
      {(detail) => <ProductEditorBody detail={detail} />}
    </AsyncSection>
  );
}

function ProductEditorBody({ detail }: { detail: ProductDetail }) {
  const { product, components, versions, milestones } = detail;
  const [name, setName] = useState(product.name);
  const [description, setDescription] = useState(product.description ?? "");
  const [isActive, setIsActive] = useState(product.isActive);
  const [allowsUnconfirmed, setAllowsUnconfirmed] = useState(product.allowsUnconfirmed);

  // `productStructureChanged` is a PREFIX over `products` and `reports`, so it
  // drops this product's own detail, the browser's list and the open-issue
  // rollup together. That chain used to be an `onChanged` handed down three
  // components deep, and each new writer had to be wired into it by hand.
  const save = useAppMutation(
    (input: Parameters<typeof updateProduct>[0]) => updateProduct(input),
    () => invalidatedBy.productStructureChanged(),
  );
  const busy = save.isPending;
  const error = save.error;

  /**
   * Deleting takes the product's issues with it, so this asks twice.
   *
   * The server refuses the first call and answers with the issue count; the
   * second passes `deleteIssues`. That is deliberately the same two-step a
   * script gets, rather than a UI-only confirm over a one-shot endpoint --
   * the guard lives where the data does.
   *
   * `confirming` holds the server's refusal message, so the confirmation
   * quotes the real count rather than the page's guess at it.
   */
  const [confirming, setConfirming] = useState<string | null>(null);
  const remove = useAppMutation(
    (input: { id: string; deleteIssues?: boolean }) => deleteProduct(input),
    () => invalidatedBy.productStructureChanged(),
  );

  return (
    <div className="flex min-w-0 flex-col flex-nowrap items-stretch justify-start gap-4 [&>*]:min-h-0 [&>*]:min-w-0">
      {/* LABELLED. These two were bare <Input>s with neither a label nor a
          placeholder -- the only thing distinguishing the name field from the
          description field was which one had more text in it. */}
      <div className="flex min-w-0 flex-col flex-nowrap items-stretch justify-start gap-3 [&>*]:min-h-0 [&>*]:min-w-0">
        <Field.Root>
          <Field.Label>Name</Field.Label>
          <Input value={name} onChange={(e) => setName(e.target.value)} />
        </Field.Root>
        <Field.Root>
          <Field.Label>Description</Field.Label>
          <Input value={description} onChange={(e) => setDescription(e.target.value)} />
        </Field.Root>
        <Checkbox
          checked={isActive}
          onCheckedChange={(next) => setIsActive(next === true)}
          label="Active"
        />
        <Checkbox
          checked={allowsUnconfirmed}
          onCheckedChange={(next) => setAllowsUnconfirmed(next === true)}
          label="Allows UNCONFIRMED"
        />
        <div>
          <Button
            variant="filled"
            disabled={busy}
            onClick={() =>
              save.mutate({
                id: product.id,
                changes: {
                  name: name.trim(),
                  description: description || undefined,
                  isActive,
                  allowsUnconfirmed,
                },
              })
            }
          >
            Save product
          </Button>
        </div>
        {error ? <FieldError>{errorMessage(error)}</FieldError> : null}

        {/* Apart from Save, and quieter than it. A destructive action sitting
            next to the primary one at the same weight is how it gets pressed
            by accident; this is the only control on the page that cannot be
            undone. */}
        <div>
          {confirming === null ? (
            <Button
              variant="plain"
              disabled={remove.isPending}
              onClick={async () => {
                try {
                  await remove.mutateAsync({ id: product.id });
                } catch (err) {
                  // The refusal IS the information: it carries the count.
                  setConfirming(errorMessage(err));
                }
              }}
            >
              Delete product
            </Button>
          ) : (
            <div className="flex min-w-0 flex-col flex-nowrap items-stretch justify-start gap-2 [&>*]:min-h-0 [&>*]:min-w-0">
              {/* The server refusal minus its last sentence. That sentence tells
                  an API caller to pass `deleteIssues`, which is right for a
                  script and wrong here: the buttons below ARE the
                  confirmation, and telling someone to pass a parameter they
                  cannot see is the interface describing itself rather than
                  their choice. The count, which is the part that matters,
                  stays. */}
              <FieldError>{confirming.replace(/\s*Pass deleteIssues to confirm\.?$/, "")}</FieldError>
              <div className="flex min-w-0 flex-row flex-wrap items-center justify-start gap-2">
                <Button
                  variant="filled"
                  intent="destructive"
                  disabled={remove.isPending}
                  onClick={() => remove.mutate({ id: product.id, deleteIssues: true })}
                >
                  Delete it and its issues
                </Button>
                <Button variant="plain" onClick={() => setConfirming(null)}>
                  Keep it
                </Button>
              </div>
            </div>
          )}
        </div>
      </div>

      {/* Stacked, not three columns. The 3-up grid was laid out for the width
          of a page; here each one is a short list over a one-field form and
          reads down the panel. */}
      <div className="mt-4 grid grid-cols-1 gap-4">
        <ComponentsAdmin productId={product.id} components={components} />
        <VersionsAdmin productId={product.id} versions={versions} />
        <MilestonesAdmin productId={product.id} milestones={milestones} />
      </div>
    </div>
  );
}

function ComponentsAdmin({
  productId,
  components,
}: {
  productId: string;
  components: ProductDetail["components"];
}) {
  const [name, setName] = useState("");

  const add = useAppMutation(
    (componentName: string) => createComponent({ productId, name: componentName }),
    () => invalidatedBy.productStructureChanged(),
  );
  const toggle = useAppMutation(
    (args: { id: string; isActive: boolean }) =>
      updateComponent({ id: args.id, changes: { isActive: !args.isActive } }),
    () => invalidatedBy.productStructureChanged(),
  );
  const busy = add.isPending || toggle.isPending;
  const error = add.error ?? toggle.error;

  const submitAdd = () => {
    if (!name.trim()) return;
    // Only the field-clearing survives; the parent refetch it used to trigger
    // is the invalidation above.
    add.mutate(name.trim(), { onSuccess: () => setName("") });
  };

  return (
    <ProductAdminColumn>
      <SectionHeading level={3}>Components</SectionHeading>
      {components.length === 0 ? (
        <Hint>None yet.</Hint>
      ) : (
        <ProductAdminList>
          {components.map((c) => (
            <ProductAdminListItem key={c.id}>
              <span>
                {c.name} {!c.isActive ? <Muted>(inactive)</Muted> : null}
              </span>
              <Button
                variant="gray"
                disabled={busy}
                onClick={() => toggle.mutate({ id: c.id, isActive: c.isActive })}
              >
                {c.isActive ? "Deactivate" : "Activate"}
              </Button>
            </ProductAdminListItem>
          ))}
        </ProductAdminList>
      )}
      <InlineForm>
        <Input aria-label="New component" placeholder="New component" value={name} onChange={(e) => setName(e.target.value)} />
        <Button variant="gray" disabled={busy || !name.trim()} onClick={submitAdd}>
          Add
        </Button>
      </InlineForm>
      {error ? <FieldError>{errorMessage(error)}</FieldError> : null}
    </ProductAdminColumn>
  );
}

function VersionsAdmin({
  productId,
  versions,
}: {
  productId: string;
  versions: ProductDetail["versions"];
}) {
  const [name, setName] = useState("");

  const add = useAppMutation(
    (versionName: string) => createVersion({ productId, name: versionName }),
    () => invalidatedBy.productStructureChanged(),
  );
  const busy = add.isPending;
  const error = add.error;

  const submitAdd = () => {
    if (!name.trim()) return;
    add.mutate(name.trim(), { onSuccess: () => setName("") });
  };

  return (
    <ProductAdminColumn>
      <SectionHeading level={3}>Versions</SectionHeading>
      {versions.length === 0 ? (
        <Hint>None yet.</Hint>
      ) : (
        <ProductAdminList>
          {versions.map((v) => (
            <ProductAdminListItem key={v.id}>{v.name}</ProductAdminListItem>
          ))}
        </ProductAdminList>
      )}
      <InlineForm>
        <Input aria-label="New version" placeholder="New version" value={name} onChange={(e) => setName(e.target.value)} />
        <Button variant="gray" disabled={busy || !name.trim()} onClick={submitAdd}>
          Add
        </Button>
      </InlineForm>
      {error ? <FieldError>{errorMessage(error)}</FieldError> : null}
      <Hint>No versions.update RPC exists yet -- versions can be created but not edited.</Hint>
    </ProductAdminColumn>
  );
}

function MilestonesAdmin({
  productId,
  milestones,
}: {
  productId: string;
  milestones: ProductDetail["milestones"];
}) {
  const [name, setName] = useState("");

  const add = useAppMutation(
    (milestoneName: string) => createMilestone({ productId, name: milestoneName }),
    () => invalidatedBy.productStructureChanged(),
  );
  const busy = add.isPending;
  const error = add.error;

  const submitAdd = () => {
    if (!name.trim()) return;
    add.mutate(name.trim(), { onSuccess: () => setName("") });
  };

  return (
    <ProductAdminColumn>
      <SectionHeading level={3}>Milestones</SectionHeading>
      {milestones.length === 0 ? (
        <Hint>None yet.</Hint>
      ) : (
        <ProductAdminList>
          {milestones.map((m) => (
            <ProductAdminListItem key={m.id}>{m.name}</ProductAdminListItem>
          ))}
        </ProductAdminList>
      )}
      <InlineForm>
        <Input aria-label="New milestone" placeholder="New milestone" value={name} onChange={(e) => setName(e.target.value)} />
        <Button variant="gray" disabled={busy || !name.trim()} onClick={submitAdd}>
          Add
        </Button>
      </InlineForm>
      {error ? <FieldError>{errorMessage(error)}</FieldError> : null}
      <Hint>No milestones.update RPC exists yet -- milestones can be created but not edited.</Hint>
    </ProductAdminColumn>
  );
}

export function ProductsAdminPage() {
  // Inactive products INCLUDED, which is a different question from the one the
  // issue list and the filing form ask, so it is a different cache entry. Both
  // go through `useProducts`; the argument is what keeps the answers apart.
  const productsQ = useProducts({ includeInactive: true });
  // One anonymous call for the whole tracker's open counts. See `rollupByProduct`
  // for why this procedure and not `components.list`. `null` is the whole
  // tracker, and it is the same key the reports page uses for its unfiltered
  // view, so the two share one entry rather than each fetching it.
  const openQ = useReportByComponent(null);
  // `products.list` is `auth: "anon", publiclyAccessible: true` and everything
  // else this page touches is `auth: "user"` -- `products.get` (the editor's
  // own query), `products.create`, `products.update`, the component / version /
  // milestone writers, `groups.list` and `flagTypes.list`. So the page is
  // public and almost every control on it is not: the sharpest case in the app
  // of a page you can read wearing controls you cannot use.
  //
  // Which is exactly why this page asks `useSession()` rather than wrapping in
  // `RequireSession`: a wrapper renders or it does not, and half of this page
  // is public. `isVisitor` is true ONLY once the server has answered nobody, so
  // while `users.me` is in flight the private controls stay hidden rather than
  // appearing and then vanishing.
  const session = useSession();
  // TWO booleans, not one negated. They are both false while `users.me` is in
  // flight, which is the point: the banner is a claim about the visitor and the
  // tab strip is a private control, and neither should be asserted before the
  // server has answered. `!signedIn` would have shown the visitor banner to
  // everyone for the length of that request.
  const signedIn = isSignedIn(session);
  const visitor = isVisitor(session);

  const rollups = useMemo(() => rollupByProduct(openQ.data ?? []), [openQ.data]);

  const browser = (
    <AsyncSection
      query={productsQ}
      loadingLabel="Loading products..."
      isEmpty={(data) => data.length === 0}
      emptyTitle="No products yet."
      emptyHint={signedIn ? "Create the first one." : "Sign in to create the first one."}
    >
      {(products, refreshing) => (
        <ProductBrowser
          products={products}
          rollups={rollups}
          signedIn={signedIn}
          refreshing={refreshing}
        />
      )}
    </AsyncSection>
  );

  return (
    <Page>
      {/* "Products", not "Products administration". The page is named for what
          most visits do on it, which is browse -- administering one is a
          drawer you open from a row, and the tracker-wide administration is
          the second tab. */}
      <PageHeader>
        <PageHeader.Title>Products</PageHeader.Title>
        <PageHeader.Description>
          What an issue can be filed against. Open one to see its components, versions and
          milestones.
        </PageHeader.Description>
      </PageHeader>
      {/* Said once at the top, the way the issue page says it, rather than
          discovered one 401 at a time. */}
      {visitor ? (
        <Banner intent="info" title="You are not signed in">
          The product list is public, so you can see what issues can be filed against.
          Sign in to create or edit a product, or to administer groups and flag types.
        </Banner>
      ) : null}

      {/* Groups and flag types are administration of the TRACKER, not of a
          product: a group restricts what anyone can see and a flag type is
          workflow vocabulary. They were rendered as two more headed sections
          below the product list, so the page titled after products was three
          unrelated admin surfaces stacked. There is no other page for them to
          live on, so they get the second tab -- reachable in one click, and
          not in front of the browsing that every visit starts with.

          Without a session there is no tab strip at all: both `groups.list`
          and `flagTypes.list` are `auth: "user"`, so the tab could only lead
          to two errors. Keyed on `signedIn` rather than on `!visitor`, so it
          is also absent while `users.me` is still in flight -- a tab that
          appears and then disappears is worse than one that arrives late. */}
      {signedIn ? (
        <Tabs defaultValue="browse" lazyMount>
          <Tabs.List>
            <Tabs.Tab value="browse">Products</Tabs.Tab>
            <Tabs.Tab value="admin">Administration</Tabs.Tab>
            <Tabs.Indicator />
          </Tabs.List>
          <Tabs.Panel value="browse">{browser}</Tabs.Panel>
          <Tabs.Panel value="admin">
            <GroupsAdmin />
            <FlagTypesAdmin />
          </Tabs.Panel>
        </Tabs>
      ) : (
        browser
      )}
    </Page>
  );
}
