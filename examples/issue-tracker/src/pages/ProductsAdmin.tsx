import { useState } from "react";
import { Button, Checkbox, Input } from "@zeroship/ui";
import {
  createComponent,
  createMilestone,
  createProduct,
  createVersion,
  getProduct,
  listProducts,
  updateComponent,
  updateProduct,
} from "../api";
import { FlagTypesAdmin } from "../components/FlagTypesAdmin";
import { GroupsAdmin } from "../components/GroupsAdmin";
import { AsyncSection } from "../components/StateViews";
import { errorMessage, useAsync } from "../components/rpc";
import type { ProductDetail } from "../components/types";

function NewProductForm({ onCreated }: { onCreated: () => void }) {
  const [name, setName] = useState("");
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
        description: description || undefined,
        classification,
        allowsUnconfirmed,
      });
      setName("");
      setDescription("");
      onCreated();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form
      className="inline-form"
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <Input aria-label="Product name" placeholder="Product name" value={name} onChange={(e) => setName(e.target.value)} required />
      <Input aria-label="Description" placeholder="Description" value={description} onChange={(e) => setDescription(e.target.value)} />
      <Input
        aria-label="Classification"
        placeholder="Classification"
        value={classification}
        onChange={(e) => setClassification(e.target.value)}
      />
      <Checkbox
        checked={allowsUnconfirmed}
        onCheckedChange={(next) => setAllowsUnconfirmed(next === true)}
        label="Allows UNCONFIRMED"
      />
      <Button type="submit" variant="filled" size="small" disabled={busy || !name.trim()}>
        Create product
      </Button>
      {error ? <p className="field-error">{error}</p> : null}
    </form>
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
    <div className="product-editor">
      <div className="inline-form">
        <Input value={name} onChange={(e) => setName(e.target.value)} />
        <Input value={description} onChange={(e) => setDescription(e.target.value)} placeholder="Description" />
        <Checkbox checked={isActive} onCheckedChange={(next) => setIsActive(next === true)} label="Active" />
        <Checkbox
          checked={allowsUnconfirmed}
          onCheckedChange={(next) => setAllowsUnconfirmed(next === true)}
          label="Allows UNCONFIRMED"
        />
        <Button variant="filled" size="small" disabled={busy} onClick={() => void save()}>
          Save product
        </Button>
      </div>
      {error ? <p className="field-error">{error}</p> : null}

      <div className="admin-grid">
        <ComponentsAdmin productId={product.id} components={components} onChanged={onChanged} />
        <VersionsAdmin productId={product.id} versions={versions} onChanged={onChanged} />
        <MilestonesAdmin productId={product.id} milestones={milestones} onChanged={onChanged} />
      </div>
    </div>
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
              {c.name} {!c.isActive ? <span className="dim">(inactive)</span> : null}
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
  const [selected, setSelected] = useState<string | null>(null);

  return (
    <div className="page products-admin-page">
      <h1>Products administration</h1>
      <GroupsAdmin />
      <FlagTypesAdmin />
      <NewProductForm onCreated={reload} />
      <AsyncSection
        state={state}
        onRetry={reload}
        loadingLabel="Loading products..."
        isEmpty={(data) => data.length === 0}
        emptyTitle="No products yet."
        emptyHint="Create the first one above."
      >
        {(products) => (
          <div className="products-admin-layout">
            <ul className="product-list">
              {products.map((p) => (
                <li key={p.id}>
                  <Button className={selected === p.id ? "btn ghost small active" : "btn ghost small"}
                    onClick={() => setSelected(p.id)}
                  >
                    {p.name} {!p.isActive ? <span className="dim">(inactive)</span> : null}
                  </Button>
                </li>
              ))}
            </ul>
            <div className="product-detail">
              {selected ? (
                <ProductEditor productId={selected} onChanged={reload} />
              ) : (
                <p className="state-hint">Select a product to manage its structure.</p>
              )}
            </div>
          </div>
        )}
      </AsyncSection>
    </div>
  );
}
