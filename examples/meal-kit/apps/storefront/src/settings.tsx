import { Card } from "@gather/meal-kit/components/ui/card";
import { FieldSet, FieldLegend } from "@gather/meal-kit/components/ui/field";
import { Label } from "@gather/meal-kit/components/ui/label";
import { Checkbox } from "@gather/meal-kit/components/ui/checkbox";
import type { MessageDescriptor } from "@lingui/core";
import { DeliveryAddress } from "@gather/meal-kit/components/delivery-address";
import { useState, type ReactNode } from "react";
import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import { Check, MapPin, Pencil, Plus, Trash2, Download } from "lucide-react";
import { SignIn } from "./customer";
import { AddressFields } from "./components/address-fields";
import { ChoiceGroup } from "@gather/meal-kit/components/choice-group";
import { useGather } from "./state";
import { AccountNav } from "./components/account-nav";
import {
  Badge,
  Button,
  Dialog,
  DialogContent,
  DialogTitle,
  DialogDescription,
  Empty,
  ErrorState,
  Field,
  Input,
  Loading,
  SectionTitle,
  Select,
  useLoad,
} from "@gather/meal-kit/components/shared";
import {
  addressSchema,
  addressValidationMessage,
  type Address,
} from "@gather/meal-kit/domain";
import {
  preferencesSchema,
  cuisines,
  kitchenEquipment,
  type Preferences,
} from "@gather/meal-kit/account-domain";
import { markets } from "@gather/meal-kit/catalog";
import * as api from "./api";

type Account = Awaited<ReturnType<typeof api.getAccount>>;
type SavedAddress = Account["addresses"][number];
function SettingsShell({
  title,
  children,
}: {
  title: string;
  children: ReactNode;
}) {
  return (
    <SignIn>
      <section className="section">
        <SectionTitle title={title} />
        <AccountNav />
        {children}
      </section>
    </SignIn>
  );
}

export function AddressesPage() {
  const { _: t } = useLingui();
  return (
    <SettingsShell title={t(msg`Saved addresses`)}>
      <Addresses />
    </SettingsShell>
  );
}
function Addresses() {
  const { _: t } = useLingui();
  const { market, act, busy, session, notice } = useGather();
  const result = useLoad(async () => api.getAccount({ market }), [market]);
  const [editing, setEditing] = useState<SavedAddress | "new" | null>(null);
  const [deleting, setDeleting] = useState<SavedAddress | null>(null);
  if (result.error)
    return <ErrorState error={result.error} retry={result.refresh} />;
  if (!result.data) return <Loading />;
  const empty: Address = {
    country: markets[market].country,
    province: market === "us" ? "NY" : "",
    district: "",
    name: session?.user?.name ?? "",
    email: session?.user?.email ?? "",
    line: "",
    city: market === "cn" ? "" : t(markets[market].region),
    postal: "",
    phone: "",
    instructions: "",
  };
  return (
    <>
      <div className="flex justify-between gap-4 flex-wrap mb-6">
        <p className="text-sm text-muted-foreground max-w-xl">
          {t(
            msg`Save an address for faster checkout. Updating it won't change existing deliveries.`,
          )}
        </p>
        <Button onClick={() => setEditing("new")}>
          <Plus size={16} />
          {t(msg`Add address`)}
        </Button>
      </div>
      {result.data.addresses.length ? (
        <div className="grid md:grid-cols-2 gap-5">
          {result.data.addresses.map((row) => {
            const address = addressSchema.parse(row.address);
            return (
              <article className="panel" key={row.id} aria-label={row.label}>
                <div className="flex items-center gap-3 mb-4">
                  <MapPin size={19} />
                  <h2 className="text-xl">{row.label}</h2>
                  {row.is_default && <Badge>{t(msg`Default address`)}</Badge>}
                </div>
                <DeliveryAddress address={address} />
                <div className="flex flex-wrap gap-2 mt-5">
                  <Button variant="outline" onClick={() => setEditing(row)}>
                    <Pencil size={15} />
                    {t(msg`Edit`)}
                  </Button>
                  {!row.is_default && (
                    <Button
                      variant="outline"
                      disabled={busy}
                      onClick={() =>
                        act(async () => {
                          await api.saveAddress({
                            id: row.id,
                            version: row.version,
                            market,
                            label: row.label,
                            address,
                            isDefault: true,
                          });
                          result.refresh();
                        })
                      }
                    >
                      <Check size={15} />
                      {t(msg`Make default`)}
                    </Button>
                  )}
                  <Button variant="ghost" onClick={() => setDeleting(row)}>
                    <Trash2 size={15} />
                    {t(msg`Remove`)}
                  </Button>
                </div>
              </article>
            );
          })}
        </div>
      ) : (
        <Empty
          title={t(msg`No saved addresses yet`)}
          text={t(msg`Add a delivery address to use at checkout.`)}
        />
      )}
      <Dialog
        open={editing !== null}
        onOpenChange={(open) => {
          if (!open) setEditing(null);
        }}
      >
        <DialogContent className="address-dialog">
          <DialogTitle>
            {editing === "new" ? t(msg`Add address`) : t(msg`Edit address`)}
          </DialogTitle>
          <DialogDescription>
            {t(msg`Enter the address where you'd like your box delivered.`)}
          </DialogDescription>
          {editing && (
            <AddressEditor
              key={
                typeof editing === "string"
                  ? editing
                  : `${editing.id}:${editing.version}`
              }
              initial={
                editing === "new" ? empty : addressSchema.parse(editing.address)
              }
              label={editing === "new" ? "" : editing.label}
              isDefault={
                editing === "new"
                  ? !result.data.addresses.length
                  : editing.is_default
              }
              onSave={async (address, label, isDefault) => {
                await api.saveAddress({
                  ...(editing === "new"
                    ? {}
                    : { id: editing.id, version: editing.version }),
                  market,
                  label,
                  address,
                  isDefault,
                });
                setEditing(null);
                result.refresh();
                notice(t(msg`Address saved.`));
              }}
            />
          )}
        </DialogContent>
      </Dialog>
      <Dialog
        open={deleting !== null}
        onOpenChange={(open) => {
          if (!open) setDeleting(null);
        }}
      >
        <DialogContent>
          <DialogTitle>{t(msg`Remove this saved address?`)}</DialogTitle>
          <DialogDescription>
            {t(
              msg`This removes the saved address. Existing deliveries and your weekly plan keep their current address.`,
            )}
          </DialogDescription>
          <Button
            disabled={busy}
            onClick={() =>
              act(async () => {
                await api.deleteAddress({
                  id: deleting!.id,
                  version: deleting!.version,
                });
                setDeleting(null);
                result.refresh();
              })
            }
          >
            {t(msg`Remove address`)}
          </Button>
        </DialogContent>
      </Dialog>
    </>
  );
}

function AddressEditor({
  initial,
  label: initialLabel,
  isDefault: initialDefault,
  onSave,
}: {
  initial: Address;
  label: string;
  isDefault: boolean;
  onSave: (
    address: Address,
    label: string,
    isDefault: boolean,
  ) => Promise<void>;
}) {
  const { _: t } = useLingui();
  const { act, busy } = useGather();
  const [address, setAddress] = useState(initial);
  const [label, setLabel] = useState(initialLabel);
  const [isDefault, setDefault] = useState(initialDefault);
  const [error, setError] = useState<MessageDescriptor | null>(null);
  const [errorField, setErrorField] = useState("");
  return (
    <form
      className="mt-5"
      noValidate
      onSubmit={(event) => {
        event.preventDefault();
        if (!label.trim()) {
          setError(msg`Give this address a name, such as Home or Work.`);
          setErrorField("label");
          (
            event.currentTarget.elements.namedItem("label") as HTMLElement
          )?.focus();
          return;
        }
        const parsed = addressSchema.safeParse(address);
        if (!parsed.success) {
          setError({ id: addressValidationMessage(parsed.error) });
          setErrorField(String(parsed.error.issues[0]?.path[0]));
          (
            event.currentTarget.elements.namedItem(
              String(parsed.error.issues[0]?.path[0]),
            ) as HTMLElement | null
          )?.focus();
          return;
        }
        setError(null);
        void act(() => onSave(parsed.data, label, isDefault));
      }}
    >
      <Field
        label={t(msg`Address label`)}
        error={errorField === "label" && error ? t(error) : undefined}
      >
        <Input
          name="label"
          value={label}
          onChange={(e) => {
            setLabel(e.target.value);
            setError(null);
          }}
          required
          maxLength={60}
          placeholder={t(msg`For example, home or work`)}
        />
      </Field>
      <AddressFields
        value={address}
        errorField={errorField}
        error={error ? t(error) : undefined}
        onChange={(next) => {
          setAddress(next);
          setError(null);
        }}
      />
      <Label className="flex gap-3 text-sm my-5">
        <Checkbox checked={isDefault} onCheckedChange={(e) => setDefault(e)} />
        {t(msg`Make this my default address`)}
      </Label>
      <Button type="submit" disabled={busy}>
        {t(msg`Save address`)}
      </Button>
    </form>
  );
}

export function PreferencesPage() {
  const { _: t } = useLingui();
  return (
    <SettingsShell title={t(msg`Your food preferences`)}>
      <PreferencesLoader />
    </SettingsShell>
  );
}
function PreferencesLoader() {
  const { market } = useGather();
  const result = useLoad(async () => api.getAccount({ market }), [market]);
  if (result.error)
    return <ErrorState error={result.error} retry={result.refresh} />;
  if (!result.data) return <Loading />;
  return (
    <PreferencesEditor
      initial={preferencesSchema.parse(result.data.profile?.preferences ?? {})}
    />
  );
}
function PreferencesEditor({ initial }: { initial: Preferences }) {
  const { _: t } = useLingui();
  const { act, busy, notice, setCart, savePreferences } = useGather();
  const [value, setValue] = useState(initial);
  const [apply, setApply] = useState(false);
  const labels: Record<string, string> = {
    milk: t(msg`Milk`),
    wheat: t(msg`Wheat`),
    nuts: t(msg`Nuts`),
    fish: t(msg`Fish`),
    soy: t(msg`Soy`),
    sesame: t(msg`Sesame`),
    mediterranean: t(msg`Mediterranean`),
    east_asian: t(msg`East Asian`),
    south_asian: t(msg`South Asian`),
    middle_eastern: t(msg`Middle Eastern`),
    american: t(msg`American`),
    oven: t(msg`Oven`),
    hob: t(msg`Stovetop`),
    microwave: t(msg`Microwave`),
    blender: t(msg`Blender`),
  };
  function choices(
    field: "exclude" | "cuisines" | "equipment",
    options: readonly string[],
    title: string,
  ) {
    return (
      <FieldSet className="mb-7">
        <FieldLegend className="font-medium mb-3">{title}</FieldLegend>
        <div className="flex flex-wrap gap-3">
          {options.map((id) => (
            <Label
              key={id}
              className="flex gap-2 items-center border border-border rounded-full px-4 py-2 text-sm"
            >
              <Checkbox
                checked={(value[field] as string[]).includes(id)}
                onCheckedChange={(e) =>
                  setValue((current) => ({
                    ...current,
                    [field]: e
                      ? [...current[field], id]
                      : current[field].filter((entry) => entry !== id),
                  }))
                }
              />
              {labels[id]}
            </Label>
          ))}
        </div>
      </FieldSet>
    );
  }
  return (
    <form
      className="panel max-w-3xl"
      onSubmit={(event) => {
        event.preventDefault();
        void act(async () => {
          const { favorites: _, ...patch } = value;
          await savePreferences(patch);
          if (apply) setCart((cart) => ({ ...cart, exclude: value.exclude }));
          notice(t(msg`Preferences saved.`));
        });
      }}
    >
      <p className="text-sm text-muted-foreground mb-7">
        {t(
          msg`Save what you like and what you'd prefer to avoid. Always check each recipe's allergen information.`,
        )}
      </p>
      {choices(
        "exclude",
        ["milk", "wheat", "nuts", "fish", "soy", "sesame"],
        t(msg`Ingredients to avoid`),
      )}
      {choices("cuisines", cuisines, t(msg`Cuisines you enjoy`))}
      {choices(
        "equipment",
        kitchenEquipment,
        t(msg`Equipment in your kitchen`),
      )}
      <ChoiceGroup
        columns
        label={t(msg`Cooking units`)}
        value={value.units}
        onChange={(units) => setValue({ ...value, units })}
        options={[
          { value: "metric", label: t(msg`Metric`) },
          { value: "us", label: t(msg`US measures`) },
          { value: "imperial", label: t(msg`UK measures`) },
        ]}
      />
      <Label className="flex gap-3 text-sm my-5">
        <Checkbox
          checked={value.marketing}
          onCheckedChange={(e) => setValue({ ...value, marketing: e })}
        />
        {t(msg`Send me optional news and offers`)}
      </Label>
      <Label className="flex gap-3 text-sm my-5">
        <Checkbox checked={apply} onCheckedChange={(e) => setApply(e)} />
        {t(msg`Apply these exclusions to my current box`)}
      </Label>
      <Button type="submit" disabled={busy}>
        {t(msg`Save preferences`)}
      </Button>
    </form>
  );
}

export function PrivacyPage() {
  const { _: t } = useLingui();
  return (
    <SettingsShell title={t(msg`Privacy and data`)}>
      <Privacy />
    </SettingsShell>
  );
}
function Privacy() {
  const { _: t } = useLingui();
  const { market, act, busy, locale } = useGather();
  const result = useLoad(async () => api.getAccount({ market }), [market]);
  const [confirmDeletion, setConfirmDeletion] = useState(false);
  if (result.error)
    return <ErrorState error={result.error} retry={result.refresh} />;
  if (!result.data) return <Loading />;
  const status: Record<string, string> = {
    requested: t(msg`Requested`),
    completed: t(msg`Completed`),
    canceled: t(msg`Canceled`),
    reviewing: t(msg`Under review`),
  };
  async function request(kind: "export" | "deletion") {
    await api.requestPrivacy({ kind, requestKey: crypto.randomUUID() });
    setConfirmDeletion(false);
    result.refresh();
  }
  return (
    <>
      <div className="grid md:grid-cols-2 gap-5 mb-8">
        <Card className="panel">
          <h2 className="text-2xl mb-3">{t(msg`Download your data`)}</h2>
          <p className="text-sm text-muted-foreground mb-5">
            {t(
              msg`Get a copy of your profile, addresses, subscriptions, orders and support history.`,
            )}
          </p>
          <Button disabled={busy} onClick={() => act(() => request("export"))}>
            {t(msg`Request data export`)}
          </Button>
        </Card>
        <Card className="panel">
          <h2 className="text-2xl mb-3">{t(msg`Request account deletion`)}</h2>
          <p className="text-sm text-muted-foreground mb-5">
            {t(
              msg`Ask us to delete your account data. Cancel your plans and upcoming orders separately. Some order records may need to be kept.`,
            )}
          </p>
          <Button
            variant="outline"
            disabled={busy}
            onClick={() => setConfirmDeletion(true)}
          >
            {t(msg`Request deletion`)}
          </Button>
        </Card>
      </div>
      <h2 className="text-2xl mb-5">{t(msg`Your requests`)}</h2>
      {result.data.privacyRequests.length ? (
        <div className="space-y-4">
          {result.data.privacyRequests.map((request) => (
            <article
              key={request.id}
              className="panel flex justify-between gap-5 flex-wrap"
            >
              <div>
                <h3 className="font-medium">
                  {request.kind === "export"
                    ? t(msg`Data export`)
                    : t(msg`Account deletion`)}
                </h3>
                <Badge>{status[request.status] ?? request.status}</Badge>
                <ol className="text-xs text-muted-foreground mt-3 space-y-1">
                  {(request.history as { at: string; status: string }[]).map(
                    (entry, index) => (
                      <li key={index}>
                        {new Intl.DateTimeFormat(locale, {
                          dateStyle: "medium",
                          timeStyle: "short",
                        }).format(new Date(entry.at))}{" "}
                        · {status[entry.status] ?? entry.status}
                      </li>
                    ),
                  )}
                </ol>
              </div>
              <div className="flex flex-wrap gap-2 items-start">
                {request.kind === "export" &&
                  ["requested", "completed"].includes(request.status) && (
                    <Button
                      disabled={busy}
                      onClick={() =>
                        act(async () => {
                          const { content } = await api.downloadPrivacyExport({
                            id: request.id,
                          });
                          const url = URL.createObjectURL(
                            new Blob([content], { type: "application/json" }),
                          );
                          const anchor = document.createElement("a");
                          anchor.href = url;
                          anchor.download = "gather-personal-data.json";
                          anchor.click();
                          setTimeout(() => URL.revokeObjectURL(url), 1000);
                          result.refresh();
                        })
                      }
                    >
                      <Download size={16} />
                      {t(msg`Download data`)}
                    </Button>
                  )}
                {request.status === "requested" && (
                  <Button
                    variant="ghost"
                    disabled={busy}
                    onClick={() =>
                      act(async () => {
                        await api.cancelPrivacyRequest({ id: request.id });
                        result.refresh();
                      })
                    }
                  >
                    {t(msg`Cancel request`)}
                  </Button>
                )}
              </div>
            </article>
          ))}
        </div>
      ) : (
        <p className="text-sm text-muted-foreground">
          {t(msg`No requests yet.`)}
        </p>
      )}
      <Dialog open={confirmDeletion} onOpenChange={setConfirmDeletion}>
        <DialogContent>
          <DialogTitle>{t(msg`Submit a deletion request?`)}</DialogTitle>
          <DialogDescription>
            {t(
              msg`We'll review your request. Your plans and orders will continue until you cancel them separately.`,
            )}
          </DialogDescription>
          <Button
            disabled={busy}
            onClick={() => act(() => request("deletion"))}
          >
            {t(msg`Submit request`)}
          </Button>
        </DialogContent>
      </Dialog>
    </>
  );
}
