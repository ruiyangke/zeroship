import { useMemo, useState } from "react";
import {
  Badge,
  Banner,
  Button,
  Checkbox,
  Dialog,
  Drawer,
  Field,
  FilterBar,
  Input,
  ListView,
  PageHeader,
  Pagination,
  Select,
  Stack,
  Tabs,
} from "@zeroship/ui";
import {
  createComponent,
  createMilestone,
  createProduct,
  createVersion,
  getProduct,
  listProducts,
  reportByComponent,
  updateComponent,
  updateProduct,
} from "../api";
import { FlagTypesAdmin } from "../components/FlagTypesAdmin";
import { GroupsAdmin } from "../components/GroupsAdmin";
import { AsyncSection } from "../components/StateViews";
import { errorMessage, useAsync } from "../components/rpc";
import { isSignedIn, isVisitor, useSession } from "../components/session";
import type { Product, ProductDetail, ReportByComponent } from "../components/types";

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

function ComponentChips({ rollup }: { rollup: Rollup }) {
  if (rollup.components.length === 0) return null;
  const shown = rollup.components.slice(0, CHIPS_PER_ROW);
  const rest = rollup.components.length - shown.length;
  return (
    <span className="product-components">
      {shown.map((component) => (
        <Badge key={component.name} intent="neutral" variant="soft" size="sm">
          {component.name} {component.count}
        </Badge>
      ))}
      {rest > 0 ? <span className="dim small">+{rest} more</span> : null}
    </span>
  );
}

function NewProductDialog({ onCreated }: { onCreated: () => void }) {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const [key, setKey] = useState("");
  const [description, setDescription] = useState("");
  const [classification, setClassification] = useState("Unclassified");
  const [allowsUnconfirmed, setAllowsUnconfirmed] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async () => {
    if (!name.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await createProduct({
        name: name.trim(),
        key: key.trim() || undefined,
        description: description || undefined,
        classification,
        allowsUnconfirmed,
      });
      setName("");
      setKey("");
      setDescription("");
      setOpen(false);
      onCreated();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      {/* A plain Button driving the controlled `open`, matching how the issue
          list opens its search builder. */}
      <Button variant="filled" size="small" onClick={() => setOpen(true)}>
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
                  void submit();
                }}
              >
                <Stack gap={3}>
                  <Field>
                    <Field.Label>Name</Field.Label>
                    <Input value={name} onChange={(e) => setName(e.target.value)} required />
                  </Field>
                  <Field>
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
                  </Field>
                  <Field>
                    <Field.Label>Description</Field.Label>
                    <Input
                      value={description}
                      onChange={(e) => setDescription(e.target.value)}
                    />
                  </Field>
                  <Field>
                    <Field.Label>Classification</Field.Label>
                    <Input
                      value={classification}
                      onChange={(e) => setClassification(e.target.value)}
                    />
                  </Field>
                  <Checkbox
                    checked={allowsUnconfirmed}
                    onCheckedChange={(next) => setAllowsUnconfirmed(next === true)}
                    label="Allows UNCONFIRMED"
                  />
                  {error ? <p className="field-error">{error}</p> : null}
                </Stack>
              </form>
            </Dialog.Body>
            <Dialog.Footer>
              <Dialog.Close>Cancel</Dialog.Close>
              <Button
                type="submit"
                form="new-product-form"
                variant="filled"
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
  onChanged,
  refreshing,
}: {
  products: Product[];
  rollups: Map<string, Rollup>;
  signedIn: boolean;
  onChanged: () => void;
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
    <div className="product-browser">
      <FilterBar
        search={search}
        onSearchChange={(next) => {
          setSearch(next);
          setPage(1);
        }}
        searchPlaceholder="Search products by name, key or classification"
        actions={signedIn ? <NewProductDialog onCreated={onChanged} /> : null}
      >
        {/* No visible label, matching the search box beside it. The reports
            filters carry visible labels because there are two of them and one
            had one; here the toolbar is a search field and one picker, and a
            "Sort" label over the picker alone would be the same asymmetry the
            other way round. The value states what it is instead. */}
        <Select
          value={sort}
          onValueChange={(next) => setSort((next as SortKey) ?? "name")}
          aria-label="Sort products"
          className="filter-select"
          renderValue={(value) => (value === "open" ? "Sort: open issues" : "Sort: name")}
        >
          <Select.Item value="name">Name</Select.Item>
          <Select.Item value="open">Open issues</Select.Item>
        </Select>
      </FilterBar>

      {/* Says how many of how many, because a filtered list that finds nothing
          and a tracker with no products are different facts. */}
      <p className="state-hint small">
        {matches.length === products.length
          ? `${products.length} ${products.length === 1 ? "product" : "products"}`
          : `${matches.length} of ${products.length} products match`}
      </p>

      {matches.length === 0 ? (
        <p className="state-hint">No product matches that search.</p>
      ) : (
        <>
          <ListView
            // Compact: this is a scanning list of 100+ rows, and the roomy
            // default spends a third more page on the same two lines.
            density="compact"
            className={refreshing ? "product-list is-refetching" : "product-list"}
            items={visible.map((product) => {
              const rollup = rollups.get(product.id) ?? EMPTY_ROLLUP;
              return {
                id: product.id,
                title: (
                  <span className="product-title">
                    <span className="product-name">{product.name}</span>
                    <Badge intent="neutral" variant="outline" size="sm">
                      {product.key}
                    </Badge>
                    {product.isActive ? null : (
                      <Badge intent="warning" variant="soft" size="sm">
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
                    size="md"
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
              {selected ? <ProductEditor productId={selected} onChanged={onChanged} /> : null}
            </Drawer.Body>
          </Drawer.Content>
        </Drawer.Portal>
      </Drawer>
    </div>
  );
}

function ProductEditor({ productId, onChanged }: { productId: string; onChanged: () => void }) {
  const { state, reload } = useAsync(() => getProduct({ id: productId }), [productId]);
  return (
    <AsyncSection state={state} onRetry={reload} loadingLabel="Loading product...">
      {(detail) => (
        <ProductEditorBody
          detail={detail}
          onChanged={() => {
            reload();
            onChanged();
          }}
        />
      )}
    </AsyncSection>
  );
}

function ProductEditorBody({ detail, onChanged }: { detail: ProductDetail; onChanged: () => void }) {
  const { product, components, versions, milestones } = detail;
  const [name, setName] = useState(product.name);
  const [description, setDescription] = useState(product.description ?? "");
  const [isActive, setIsActive] = useState(product.isActive);
  const [allowsUnconfirmed, setAllowsUnconfirmed] = useState(product.allowsUnconfirmed);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await updateProduct({
        id: product.id,
        changes: { name: name.trim(), description: description || undefined, isActive, allowsUnconfirmed },
      });
      onChanged();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Stack gap={4} className="product-editor">
      {/* LABELLED. These two were bare <Input>s with neither a label nor a
          placeholder -- the only thing distinguishing the name field from the
          description field was which one had more text in it. */}
      <Stack gap={3}>
        <Field>
          <Field.Label>Name</Field.Label>
          <Input value={name} onChange={(e) => setName(e.target.value)} />
        </Field>
        <Field>
          <Field.Label>Description</Field.Label>
          <Input value={description} onChange={(e) => setDescription(e.target.value)} />
        </Field>
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
          <Button variant="filled" size="small" disabled={busy} onClick={() => void save()}>
            Save product
          </Button>
        </div>
        {error ? <p className="field-error">{error}</p> : null}
      </Stack>

      {/* Stacked, not three columns. The 3-up grid was laid out for the width
          of a page; here each one is a short list over a one-field form and
          reads down the panel. */}
      <div className="admin-grid">
        <ComponentsAdmin productId={product.id} components={components} onChanged={onChanged} />
        <VersionsAdmin productId={product.id} versions={versions} onChanged={onChanged} />
        <MilestonesAdmin productId={product.id} milestones={milestones} onChanged={onChanged} />
      </div>
    </Stack>
  );
}

function ComponentsAdmin({
  productId,
  components,
  onChanged,
}: {
  productId: string;
  components: ProductDetail["components"];
  onChanged: () => void;
}) {
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const add = async () => {
    if (!name.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await createComponent({ productId, name: name.trim() });
      setName("");
      onChanged();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const toggleActive = async (id: string, isActive: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await updateComponent({ id, changes: { isActive: !isActive } });
      onChanged();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="admin-col">
      <h3>Components</h3>
      {components.length === 0 ? (
        <p className="state-hint small">None yet.</p>
      ) : (
        <ul>
          {components.map((c) => (
            <li key={c.id}>
              <span>
                {c.name} {!c.isActive ? <span className="dim">(inactive)</span> : null}
              </span>
              <Button variant="gray" size="small" disabled={busy} onClick={() => void toggleActive(c.id, c.isActive)}>
                {c.isActive ? "Deactivate" : "Activate"}
              </Button>
            </li>
          ))}
        </ul>
      )}
      <div className="inline-form">
        <Input aria-label="New component" placeholder="New component" value={name} onChange={(e) => setName(e.target.value)} />
        <Button variant="gray" size="small" disabled={busy || !name.trim()} onClick={() => void add()}>
          Add
        </Button>
      </div>
      {error ? <p className="field-error">{error}</p> : null}
    </div>
  );
}

function VersionsAdmin({
  productId,
  versions,
  onChanged,
}: {
  productId: string;
  versions: ProductDetail["versions"];
  onChanged: () => void;
}) {
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const add = async () => {
    if (!name.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await createVersion({ productId, name: name.trim() });
      setName("");
      onChanged();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="admin-col">
      <h3>Versions</h3>
      {versions.length === 0 ? (
        <p className="state-hint small">None yet.</p>
      ) : (
        <ul>
          {versions.map((v) => (
            <li key={v.id}>{v.name}</li>
          ))}
        </ul>
      )}
      <div className="inline-form">
        <Input aria-label="New version" placeholder="New version" value={name} onChange={(e) => setName(e.target.value)} />
        <Button variant="gray" size="small" disabled={busy || !name.trim()} onClick={() => void add()}>
          Add
        </Button>
      </div>
      {error ? <p className="field-error">{error}</p> : null}
      <p className="state-hint small">No versions.update RPC exists yet -- versions can be created but not edited.</p>
    </div>
  );
}

function MilestonesAdmin({
  productId,
  milestones,
  onChanged,
}: {
  productId: string;
  milestones: ProductDetail["milestones"];
  onChanged: () => void;
}) {
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const add = async () => {
    if (!name.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await createMilestone({ productId, name: name.trim() });
      setName("");
      onChanged();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="admin-col">
      <h3>Milestones</h3>
      {milestones.length === 0 ? (
        <p className="state-hint small">None yet.</p>
      ) : (
        <ul>
          {milestones.map((m) => (
            <li key={m.id}>{m.name}</li>
          ))}
        </ul>
      )}
      <div className="inline-form">
        <Input aria-label="New milestone" placeholder="New milestone" value={name} onChange={(e) => setName(e.target.value)} />
        <Button variant="gray" size="small" disabled={busy || !name.trim()} onClick={() => void add()}>
          Add
        </Button>
      </div>
      {error ? <p className="field-error">{error}</p> : null}
      <p className="state-hint small">No milestones.update RPC exists yet -- milestones can be created but not edited.</p>
    </div>
  );
}

export function ProductsAdminPage() {
  const { state, reload } = useAsync(() => listProducts({ includeInactive: true }), []);
  // One anonymous call for the whole tracker's open counts. See `rollupByProduct`
  // for why this procedure and not `components.list`.
  const openQ = useAsync(() => reportByComponent({}), []);
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

  const rollups = useMemo(
    () => rollupByProduct(openQ.state.status === "ready" ? openQ.state.data : []),
    [openQ.state],
  );

  const reloadAll = () => {
    reload();
    openQ.reload();
  };

  const browser = (
    <AsyncSection
      state={state}
      onRetry={reload}
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
          onChanged={reloadAll}
          refreshing={refreshing}
        />
      )}
    </AsyncSection>
  );

  return (
    <div className="page products-admin-page">
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
    </div>
  );
}
