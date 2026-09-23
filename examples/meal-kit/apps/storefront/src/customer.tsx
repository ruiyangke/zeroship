import { statusLabel } from "@gather/meal-kit/order-status";
import { Card } from "@gather/meal-kit/components/ui/card";
import { Label } from "@gather/meal-kit/components/ui/label";
import { Tabs, TabsList, TabsTrigger } from "@gather/meal-kit/components/ui/tabs";
import { AnimatedTabsContent as TabsContent } from "@gather/meal-kit/components/animated-tabs-content";
import { Textarea } from "@gather/meal-kit/components/ui/textarea";
import { Checkbox } from "@gather/meal-kit/components/ui/checkbox";
import { boxSizeMessage } from "@gather/meal-kit/box-copy";
import { AddressFields } from "./components/address-fields";
import { ChoiceGroup } from "@gather/meal-kit/components/choice-group";
import { recipeText } from "@gather/meal-kit/catalog-domain";
import { DeliveryAddress } from "@gather/meal-kit/components/delivery-address";
import { formatLocale } from "@gather/meal-kit/catalog";
import type { I18n, MessageDescriptor } from "@lingui/core";
import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import { useEffect, useState, useRef, type ReactNode } from "react";
import {
  Link,
  useNavigate,
  useParams,
  useSearchParams,
} from "react-router-dom";
import { Check, LockKeyhole, Package, ArrowRight } from "lucide-react";
import { useGather } from "./state";
import { deliveryLabel, markets, money } from "@gather/meal-kit/catalog";
import {
  addressSchema,
  addressValidationMessage,
  validateAddress,
  addressDestination,
  type Address,
  type Order,
  type Cart,
} from "@gather/meal-kit/domain";
import * as api from "./api";
import {
  Badge,
  Button,
  CtaLink,
  Dialog,
  DialogContent,
  DialogDescription,
  DialogTitle,
  Empty,
  ErrorState,
  Field,
  Input,
  Loading,
  SectionTitle,
  Select,
  useLoad,
} from "@gather/meal-kit/components/shared";
import { AccountNav } from "./components/account-nav";
import { DeliveryAreaFields } from "@gather/meal-kit/components/delivery-area";
import {
  deliveryEligible,
  countryPolicies,
  emptyArea,
  destinationKey,
  chinaDistricts,
} from "@gather/meal-kit/countries";
import { BoxSummary } from "./components/box-summary";
import { PurchaseSteps } from "./components/purchase-steps";
import { useCheckoutQuote } from "./use-checkout-quote";
import { reviewBox } from "@gather/meal-kit/box-domain";

export function SignIn({ children }: { children: ReactNode }) {
  const { session, sessionError, refreshSession, act, login } = useGather();
  const { _: t } = useLingui();
  if (sessionError)
    return (
      <ErrorState
        error={sessionError}
        retry={() => {
          void act(refreshSession);
        }}
      />
    );
  if (!session) return <Loading />;
  if (!session.user)
    return (
      <Empty
        title={t(msg`Sign in to Gather`)}
        text={t(
          msg`Sign in to keep your deliveries, recipes and preferences together.`,
        )}
      >
        <Button onClick={() => act(login)}>
          {t(msg`Sign in to continue`)}
        </Button>
      </Empty>
    );
  return children;
}
export function Checkout() {
  const { cart, path, catalog, catalogError, refreshCatalog } = useGather();
  const { _: t } = useLingui();
  if (catalogError)
    return <ErrorState error={catalogError} retry={refreshCatalog} />;
  if (!catalog) return <Loading />;
  if (!reviewBox(cart, catalog).ready)
    return (
      <section className="section">
        <Empty
          title={t(msg`Finish your box`)}
          text={t(msg`Choose your delivery area and meals to continue.`)}
        >
          <CtaLink to={path("/box")}>{t(msg`Review your box`)}</CtaLink>
        </Empty>
      </section>
    );
  return (
    <SignIn>
      <CheckoutForm />
    </SignIn>
  );
}
function CheckoutForm() {
  const {
    cart,
    locale,
    market,
    session,
    act,
    busy,
    path,
    setCart,
    flushDraft,
  } = useGather();
  const { _: t } = useLingui();
  const navigate = useNavigate();
  const freshAddress: Address = {
    country: markets[market].country,
    province:
      market === "cn" ? cart.area.province : market === "us" ? "NY" : "",
    district: cart.area.district,
    name: session?.user?.name ?? "",
    email: session?.user?.email ?? "",
    line: "",
    city: market === "cn" ? cart.area.city : t(markets[market].region),
    postal: cart.postal,
    phone: "",
    instructions: "",
  };
  const [address, setAddress] = useState<Address>(freshAddress);
  const [savedAddressId, setSavedAddressId] = useState("");
  const [consentFor, setConsentFor] = useState<string | null>(null);
  const [requestKey] = useState(() => crypto.randomUUID());
  const [validation, setValidation] = useState("");
  const [validationField, setValidationField] = useState("");
  const addressTouched = useRef(false);
  const [hasPlan, setHasPlan] = useState(false);
  const [addresses, setAddresses] = useState<
    Awaited<ReturnType<typeof api.getAccount>>["addresses"]
  >([]);
  const checkoutCart = { ...cart, ...addressDestination(address) };
  const quote = useCheckoutQuote(checkoutCart);
  const quoteIdentity = quote.data
    ? JSON.stringify([
        quote.data.menuVersionId,
        quote.data.fingerprint,
        quote.data.total,
        quote.data.currency,
        cart.recurring,
      ])
    : null;
  const consent = quoteIdentity !== null && consentFor === quoteIdentity;
  useEffect(() => {
    let live = true;
    Promise.resolve(api.getAccount({ market }))
      .then((a) => {
        if (!live) return;
        setHasPlan(!!a.plan);
        setAddresses(a.addresses);
        const saved = a.addresses.find((row) => row.is_default);
        if (saved && !addressTouched.current) {
          const parsed = addressSchema.safeParse(saved.address);
          if (
            parsed.success &&
            destinationKey(market, addressDestination(parsed.data)) ===
              destinationKey(market, cart)
          ) {
            setAddress(parsed.data);
            setSavedAddressId(saved.id);
          }
        }
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, []);
  return (
    <section className="section">
      <PurchaseSteps current="checkout" />
      <SectionTitle
        eyebrow={t(msg`ALMOST AT YOUR TABLE`)}
        title={t(msg`Review your order`)}
        body={t(msg`Check your meals, delivery address and total.`)}
      />
      <div className="checkout-grid">
        <form
          className="panel"
          noValidate
          onSubmit={(e) => {
            e.preventDefault();
            const parsed = addressSchema.safeParse(address);
            if (!parsed.success) {
              setValidation(addressValidationMessage(parsed.error));
              setValidationField(String(parsed.error.issues[0]?.path[0]));
              (
                e.currentTarget.elements.namedItem(
                  String(parsed.error.issues[0]?.path[0]),
                ) as HTMLElement | null
              )?.focus();
              return;
            }
            if (!quote.data || !consent) return;
            act(async () => {
              if (!(await flushDraft())) return;
              const order = await api.checkout({
                cart: checkoutCart,
                address: parsed.data,
                quote: quote.data!,
                requestKey,
                consent: true,
              });
              setCart({ ...checkoutCart, recipeIds: [] });
              navigate(path(`/orders/${order.id}`));
            });
          }}
        >
          <h2 className="text-2xl mb-6">{t(msg`Delivery address`)}</h2>
          {addresses.length > 0 && (
            <ChoiceGroup
              label={t(msg`Use a saved address`)}
              value={savedAddressId}
              onChange={(id) => {
                addressTouched.current = true;
                setSavedAddressId(id);
                setValidation("");
                const row = addresses.find((address) => address.id === id);
                setAddress(
                  row ? addressSchema.parse(row.address) : freshAddress,
                );
              }}
              options={[
                ...addresses.map((row) => {
                  const saved = addressSchema.parse(row.address);
                  return {
                    value: row.id,
                    label: row.label,
                    description: saved.line,
                  };
                }),
                {
                  value: "",
                  label: t(msg`Enter another address`),
                  description: t(msg`Fill in the delivery details below.`),
                },
              ]}
            />
          )}
          <AddressFields
            errorField={validationField}
            error={validation ? t(validation) : undefined}
            value={address}
            onChange={(next) => {
              addressTouched.current = true;
              setAddress(next);
              setSavedAddressId("");
              setValidation("");
            }}
          />
          <p className="text-xs mb-6">
            {t(msg`Arriving`)}:{" "}
            <strong>{deliveryLabel(cart.deliveryDate, market, locale)}</strong>{" "}
            · {markets[market].timezone}
          </p>
          <h2 className="text-2xl mb-5">{t(msg`Payment`)}</h2>
          {cart.recurring && hasPlan && (
            <p className="notice">
              {t(
                msg`These choices will update your weekly plan here. Your other plans and confirmed orders stay as they are.`,
              )}
            </p>
          )}
          {quote.error ? (
            <ErrorState error={quote.error} retry={quote.refresh} />
          ) : !quote.data ? (
            <Loading />
          ) : (
            <div className="notice">
              <strong>
                {t(msg`Total due`)}: {money(quote.data.total, market, locale)}
              </strong>
              <p>
                {cart.recurring
                  ? t(
                      msg`Your choices are saved for next week. Review and confirm each future box in My deliveries before ordering.`,
                    )
                  : t(msg`One-time purchase. No recurring deliveries.`)}
              </p>
            </div>
          )}
          <Button
            type="button"
            variant="link"
            className="mb-5"
            onClick={quote.refresh}
          >
            {t(msg`Refresh total`)}
          </Button>
          <Label className="flex gap-3 items-start text-xs mb-6">
            <Checkbox
              required
              disabled={!quote.data || busy}
              checked={consent}
              onCheckedChange={(e) => setConsentFor(e ? quoteIdentity : null)}
            />
            <span>
              {cart.recurring
                ? t(
                    msg`I agree to this order and saving my choices for a weekly plan. I will review and confirm each future box before ordering.`,
                  )
                : t(
                    msg`I agree to the order terms for this one-time purchase.`,
                  )}
            </span>
          </Label>
          <Button
            type="submit"
            size="lg"
            className="w-full"
            disabled={busy || !consent || !quote.data}
          >
            {busy ? t(msg`Placing your order…`) : t(msg`Place order`)}
            <ArrowRight />
          </Button>
          <Link
            className="block text-center text-xs underline mt-4"
            to={path("/menu")}
          >
            {t(msg`Back to my meals`)}
          </Link>
        </form>
        <BoxSummary checkout quote={quote.data} />
      </div>
    </section>
  );
}

export function Account() {
  return (
    <SignIn>
      <AccountContent />
    </SignIn>
  );
}
function AccountContent() {
  const { locale, path, act, busy, market } = useGather();
  const { _: t } = useLingui();
  const navigate = useNavigate();
  const result = useLoad(async () => api.getAccount({ market }));
  const [searchParams, setSearchParams] = useSearchParams();
  const requestedTab = searchParams.get("view");
  const tab =
    requestedTab === "history" || requestedTab === "support"
      ? requestedTab
      : "upcoming";
  const [confirm, setConfirm] = useState<"cancel" | "pause" | null>(null);
  const [renewal, setRenewal] = useState<Awaited<
    ReturnType<typeof api.previewRenewal>
  > | null>(null);
  if (result.error)
    return <ErrorState error={result.error} retry={result.refresh} />;
  if (!result.data) return <Loading />;
  const { orders, plan, cases } = result.data;
  const filtered = orders
    .filter((o) =>
      tab === "history"
        ? ["completed", "canceled"].includes(o.status)
        : !["completed", "canceled"].includes(o.status),
    )
    .sort((a, b) => {
      const attention = (order: Order) =>
        ["pending_payment", "payment_recovery", "checkout_expired"].includes(
          order.status,
        )
          ? 0
          : 1;
      return tab === "upcoming"
        ? attention(a) - attention(b) ||
            a.snapshot.cart.deliveryDate.localeCompare(
              b.snapshot.cart.deliveryDate,
            )
        : b.snapshot.cart.deliveryDate.localeCompare(
            a.snapshot.cart.deliveryDate,
          );
    });
  const change = (action: "skip" | "pause" | "resume" | "cancel") =>
    act(async () => {
      await api.updatePlan({ market, action, version: plan!.version });
      setConfirm(null);
      result.refresh();
    });
  return (
    <section className="section">
      <SectionTitle eyebrow={t(msg`YOUR GATHER`)} title={t(msg`My deliveries`)}>
        <CtaLink to={path("/plans")}>{t(msg`Build another box`)}</CtaLink>
      </SectionTitle>
      <AccountNav />
      {plan && (
        <Card className="panel mb-8">
          <div className="flex justify-between items-start gap-5 flex-wrap">
            <div>
              <Badge>{t(msg`Weekly plan`)}</Badge> ·{" "}
              <span>{statusLabel(plan.status, t)}</span>
              <h2 className="text-2xl mt-4">
                {plan.status === "active"
                  ? t(msg`Next box to review`)
                  : t(msg`Your plan is`)}{" "}
                {plan.status === "active"
                  ? deliveryLabel(
                      plan.next_date,
                      (plan.configuration as { cart: Cart }).cart.market,
                      locale,
                    )
                  : statusLabel(plan.status, t)}
              </h2>
              <p className="text-xs text-muted-foreground mt-2">
                {t(
                  msg`Plan changes apply to future deliveries. Manage confirmed boxes below.`,
                )}
              </p>
            </div>
            <div className="flex gap-2 flex-wrap">
              {plan.status === "active" ? (
                <>
                  <Button
                    variant="outline"
                    disabled={busy}
                    onClick={() => change("skip")}
                  >
                    {t(msg`Skip next week`)}
                  </Button>
                  <Button variant="outline" onClick={() => setConfirm("pause")}>
                    {t(msg`Pause plan`)}
                  </Button>
                </>
              ) : plan.status === "canceled" ? (
                <CtaLink to={path("/plans")}>
                  {t(msg`Start a new plan`)}
                </CtaLink>
              ) : (
                <Button onClick={() => change("resume")}>
                  {t(msg`Resume plan`)}
                </Button>
              )}
              {plan.status !== "canceled" && (
                <Button variant="ghost" onClick={() => setConfirm("cancel")}>
                  {t(msg`Cancel plan`)}
                </Button>
              )}
            </div>
          </div>
          {plan.status === "active" && (
            <div className="mt-6 pt-5 border-t border-border flex gap-4 items-center flex-wrap">
              <p className="text-xs text-muted-foreground flex-1">
                {t(
                  msg`Your next box needs confirmation. Review the meals and total before ordering.`,
                )}
              </p>
              <Button
                variant="outline"
                disabled={busy}
                onClick={() =>
                  act(async () =>
                    setRenewal(await api.previewRenewal({ market })),
                  )
                }
              >
                {t(msg`Review next box`)}
              </Button>
            </div>
          )}
        </Card>
      )}
      <Tabs
        value={tab}
        onValueChange={(value) => setSearchParams({ view: String(value) })}
      >
        <TabsList
          variant="line"
          className="account-tabs"
          aria-label={t(msg`Account sections`)}
        >
          {[
            ["upcoming", t(msg`Upcoming boxes`)],
            ["history", t(msg`Order history`)],
            ["support", t(msg`Support requests`)],
          ].map(([id, label]) => (
            <TabsTrigger key={id} value={id}>
              {label}
            </TabsTrigger>
          ))}
        </TabsList>
        <TabsContent value={tab}>
          {tab === "support" ? (
            cases.length ? (
              cases.map((c) => (
                <Card className="panel mb-4" key={c.id}>
                  <Badge>{statusLabel(c.status, t)}</Badge>
                  <p className="my-3">{c.message}</p>
                  {c.resolution && <p className="notice">{c.resolution}</p>}
                  <Link
                    className="text-xs underline"
                    to={path(`/orders/${c.order_id}`)}
                  >
                    {t(msg`View order`)}
                  </Link>
                </Card>
              ))
            ) : (
              <Empty
                title={t(msg`No support requests`)}
                text={t(
                  msg`Need help with a delivery? Open the order and tell us what happened.`,
                )}
              />
            )
          ) : filtered.length ? (
            filtered.map((o) => <OrderCard key={o.id} order={o} />)
          ) : (
            <Empty
              title={
                tab === "history"
                  ? t(msg`No past orders yet`)
                  : orders.length
                    ? t(msg`No upcoming boxes`)
                    : t(msg`Choose your first box`)
              }
              text={
                tab === "history"
                  ? t(msg`Delivered and canceled orders will appear here.`)
                  : orders.length
                    ? t(
                        msg`Your past orders are in Order history. Build a box when you are ready for another delivery.`,
                      )
                    : t(msg`Choose meals and a delivery date to get started.`)
              }
            >
              <CtaLink to={path("/plans")}>{t(msg`Choose a box`)}</CtaLink>
            </Empty>
          )}
        </TabsContent>
      </Tabs>
      <Dialog
        open={confirm !== null}
        onOpenChange={(open) => {
          if (!open) setConfirm(null);
        }}
      >
        <DialogContent>
          <DialogTitle>
            {confirm === "pause"
              ? t(msg`Pause your weekly plan?`)
              : t(msg`Cancel your weekly plan?`)}
          </DialogTitle>
          <DialogDescription>
            {t(
              msg`Future deliveries will stop. Any confirmed boxes will still arrive unless you cancel them separately before their deadline.`,
            )}
          </DialogDescription>
          <Button disabled={busy} onClick={() => confirm && change(confirm)}>
            {t(msg`Confirm change`)}
          </Button>
          <Button variant="outline" onClick={() => setConfirm(null)}>
            {t(msg`Keep my plan`)}
          </Button>
        </DialogContent>
      </Dialog>
      <Dialog
        open={!!renewal}
        onOpenChange={(open) => {
          if (!open) setRenewal(null);
        }}
      >
        <DialogContent>
          <DialogTitle>{t(msg`Review your next box`)}</DialogTitle>
          <DialogDescription>
            {t(
              msg`This box uses your saved meals and address. Check the delivery date and total before confirming.`,
            )}
          </DialogDescription>
          {renewal && (
            <>
              <p>{deliveryLabel(renewal.date, renewal.market, locale)}</p>
              <ul className="space-y-2">
                {renewal.recipes.map((recipe) => (
                  <li key={recipe.id}>{recipeText(recipe, locale).name}</li>
                ))}
              </ul>
              <p>
                {t(
                  boxSizeMessage(renewal.cart.mealCount, renewal.cart.servings),
                )}
              </p>
              <div className="border-y border-border py-4">
                <h3 className="font-semibold mb-2">
                  {t(msg`Delivery address`)}
                </h3>
                <DeliveryAddress address={renewal.address} />
              </div>
              <strong>
                {money(renewal.quote.total, renewal.market, locale)}
              </strong>
              <Button
                disabled={busy}
                onClick={() =>
                  act(async () => {
                    const order = await api.renewPlan({
                      market,
                      date: renewal.date,
                      total: renewal.quote.total,
                    });
                    setRenewal(null);
                    navigate(path(`/orders/${order.id}`));
                  })
                }
              >
                {t(msg`Order this box`)}
              </Button>
            </>
          )}
        </DialogContent>
      </Dialog>
    </section>
  );
}
function OrderCard({ order: o }: { order: Order }) {
  const { locale, path } = useGather();
  const { _: t } = useLingui();
  return (
    <article className="order-card">
      <div>
        <Badge>{statusLabel(o.status, t)}</Badge>
        <h2 className="text-2xl mt-3">
          {deliveryLabel(o.snapshot.cart.deliveryDate, o.market, locale)}
        </h2>
        <p className="text-xs mt-1 text-muted-foreground">
          {t(
            boxSizeMessage(o.snapshot.cart.mealCount, o.snapshot.cart.servings),
          )}
        </p>
        <div className="mini-meals">
          {o.snapshot.recipes.map((r) => (
            <img
              key={r.id}
              src={`/media/${r.image}.png`}
              alt={recipeText(r, locale).name}
            />
          ))}
        </div>
      </div>
      <div className="flex flex-col justify-between gap-4 items-start">
        <strong>{money(o.total, o.market, locale)}</strong>
        {["confirmed", "completed"].includes(o.status) && (
          <span className="text-xs">{statusLabel(o.fulfillment, t)}</span>
        )}
        <CtaLink outline to={path(`/orders/${o.id}`)}>
          {t(msg`Manage box`)}
        </CtaLink>
      </div>
    </article>
  );
}

export function OrderPage() {
  return (
    <SignIn>
      <OrderContent />
    </SignIn>
  );
}
function OrderContent() {
  const { id = "" } = useParams();
  const { locale, act, busy, notice, path } = useGather();
  const { _: t, i18n } = useLingui();
  const result = useLoad(async () => api.getOrder({ id }), [id]);
  const [dialog, setDialog] = useState<
    "cancel" | "edit" | "issue" | "retry" | null
  >(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [category, setCategory] = useState<Parameters<typeof api.reportIssue>[0]["category"]>("missing");
  const [message, setMessage] = useState("");
  const retryQuote = useLoad(
    async () =>
      result.data && dialog === "retry"
        ? api.getQuote(result.data.snapshot.cart)
        : null,
    [result.data?.id, dialog],
  );
  const editMenu = useLoad(
    async () =>
      result.data && dialog === "edit"
        ? api.getCatalog({
            market: result.data.market,
            date: result.data.snapshot.cart.deliveryDate,
          })
        : null,
    [result.data?.id, dialog],
  );
  const editQuote = useLoad(
    async () =>
      result.data &&
      dialog === "edit" &&
      selected.length === result.data.snapshot.cart.mealCount
        ? api.getQuote({ ...result.data.snapshot.cart, recipeIds: selected })
        : null,
    [result.data?.id, dialog, selected.join(",")],
  );
  if (result.error)
    return <ErrorState error={result.error} retry={result.refresh} />;
  if (!result.data) return <Loading />;
  const o = result.data;
  const editable =
    o.status !== "canceled" &&
    o.fulfillment === "unallocated" &&
    Date.now() < Date.parse(o.snapshot.cutoff);
  const perform = (fn: () => unknown | Promise<unknown>) =>
    act(async () => {
      await fn();
      setDialog(null);
      result.refresh();
    });
  return (
    <section className="section">
      <SectionTitle
        eyebrow={t(msg`YOUR DELIVERY`)}
        title={
          o.status === "confirmed"
            ? t(msg`Your order is confirmed.`)
            : statusLabel(o.status, t)
        }
        body={t(
          msg`${deliveryLabel(o.snapshot.cart.deliveryDate, o.market, locale)} · ${t(boxSizeMessage(o.snapshot.cart.mealCount, o.snapshot.cart.servings))}`,
        )}
      />
      <div className="checkout-grid">
        <div>
          <Card className="panel mb-6">
            <div className="flex gap-2 flex-wrap mb-5">
              <Badge>
                {o.refunded === o.total && o.refunded > 0
                  ? t(msg`Refunded`)
                  : statusLabel(o.payment, t)}
              </Badge>
              {["confirmed", "completed"].includes(o.status) && (
                <Badge>{statusLabel(o.fulfillment, t)}</Badge>
              )}
            </div>
            {o.payment !== "succeeded" && o.status !== "canceled" && (
              <div className="notice">
                <p className="mb-3">
                  {o.status === "checkout_expired"
                    ? t(
                        msg`Your meals are no longer reserved. We're checking the payment result before you can try again.`,
                      )
                    : o.payment === "processing"
                      ? t(
                          msg`We're waiting for your payment confirmation. Please don't pay again. You can leave this page and check your order later.`,
                        )
                      : o.payment === "requires_action"
                        ? t(
                            msg`Complete payment verification to confirm your order.`,
                          )
                        : t(
                            msg`Your payment didn't go through. Your meals are saved so you can try again.`,
                          )}
                </p>
                {o.payment === "requires_action" &&
                  o.status === "pending_payment" &&
                  o.snapshot.paymentDeadline && (
                    <p className="mb-3">
                      {t(msg`Complete verification by`)}:{" "}
                      {new Intl.DateTimeFormat(formatLocale(locale, o.market), {
                        dateStyle: "medium",
                        timeStyle: "short",
                        timeZone: markets[o.market].timezone,
                      }).format(new Date(o.snapshot.paymentDeadline))}{" "}
                      ({markets[o.market].timezone})
                    </p>
                  )}
                {o.payment === "processing" ||
                o.status === "checkout_expired" ? (
                  <Button variant="outline" onClick={result.refresh}>
                    {t(msg`Check payment status`)}
                  </Button>
                ) : (
                  <Button
                    disabled={busy || !editable}
                    onClick={() =>
                      o.payment === "failed"
                        ? setDialog("retry")
                        : perform(() => api.payOrder({ id }))
                    }
                  >
                    {o.payment === "requires_action"
                      ? t(msg`Complete verification`)
                      : t(msg`Try payment again`)}
                  </Button>
                )}
              </div>
            )}
            {o.status === "payment_recovery" && (
              <div className="notice" role="status">
                {t(
                  msg`Your payment arrived after your meals were released. This box is not confirmed. Our team needs to review your payment; please don't pay again.`,
                )}
              </div>
            )}
            <h2 className="text-2xl mb-5">{t(msg`Meals`)}</h2>
            <div className="space-y-5">
              {o.snapshot.recipes.map((r) => (
                <Link
                  className="flex gap-4 items-center"
                  key={r.id}
                  to={path(`/orders/${o.id}/cook/${r.id}`)}
                >
                  <img
                    className="w-20 h-20 object-cover rounded-lg"
                    src={`/media/${r.image}.png`}
                    alt=""
                  />
                  <div>
                    <h3>{recipeText(r, locale).name}</h3>
                    <p className="text-xs text-muted-foreground">
                      {recipeText(r, locale).subtitle}
                    </p>
                  </div>
                  <ArrowRight size={16} className="ml-auto shrink-0" />
                </Link>
              ))}
            </div>
            <p className="text-xs text-muted-foreground mt-6">
              {t(msg`Change by`)}:{" "}
              {new Intl.DateTimeFormat(formatLocale(locale, o.market), {
                dateStyle: "medium",
                timeStyle: "short",
                timeZone: markets[o.market].timezone,
              }).format(new Date(o.snapshot.cutoff))}{" "}
              ({markets[o.market].timezone})
            </p>
            <div className="flex gap-3 flex-wrap mt-5">
              {editable &&
                o.payment === "succeeded" &&
                o.status === "confirmed" && (
                  <Button
                    variant="outline"
                    onClick={() => {
                      setSelected(o.snapshot.cart.recipeIds);
                      setDialog("edit");
                    }}
                  >
                    {t(msg`Edit meals`)}
                  </Button>
                )}
              {editable && (
                <Button variant="outline" onClick={() => setDialog("cancel")}>
                  {t(msg`Cancel this box`)}
                </Button>
              )}
              <Button variant="ghost" onClick={() => setDialog("issue")}>
                {t(msg`Report an issue`)}
              </Button>
              <Button
                variant="ghost"
                disabled={busy}
                onClick={() =>
                  act(async () => {
                    for (let attempt = 0; attempt < 30; attempt++) {
                      const receipt = await api.downloadReceipt({ id });
                      if (receipt.ready) {
                        const url = URL.createObjectURL(
                          new Blob([receipt.content], {
                            type: "application/json",
                          }),
                        );
                        const link = document.createElement("a");
                        link.href = url;
                        link.download = `gather-${id}.json`;
                        link.click();
                        URL.revokeObjectURL(url);
                        return;
                      }
                      await new Promise((resolve) => setTimeout(resolve, 500));
                    }
                    notice(
                      t(
                        msg`Your download is being prepared. Please try again shortly.`,
                      ),
                    );
                  })
                }
              >
                {t(msg`Download order details`)}
              </Button>
            </div>
          </Card>
          <Card className="panel">
            <h2 className="text-2xl">{t(msg`Order updates`)}</h2>
            <ol className="timeline">
              {o.timeline.map((e, i) => {
                const title = statusLabel(e.key, t);
                const detail =
                  e.key !== "support_resolved" &&
                  Object.hasOwn(i18n.messages, e.detail)
                    ? t(e.detail, e.values)
                    : e.detail;
                return (
                  <li key={i}>
                    <strong>{title}</strong>
                    {detail !== title && (
                      <p className="text-muted-foreground">{detail}</p>
                    )}
                    <time className="text-[10px]">
                      {new Date(e.at).toLocaleString(
                        formatLocale(locale, o.market),
                        { timeZone: markets[o.market].timezone },
                      )}
                    </time>
                  </li>
                );
              })}
            </ol>
          </Card>
        </div>
        <div>
          <Card className="panel mb-5">
            <h2 className="text-2xl mb-5">{t(msg`Delivery address`)}</h2>
            <DeliveryAddress address={o.snapshot.address} />
            {o.snapshot.address.instructions && (
              <p className="text-xs text-muted-foreground mt-4">
                {o.snapshot.address.instructions}
              </p>
            )}
          </Card>
          <Card className="panel">
            <h2 className="text-2xl mb-5">{t(msg`Order summary`)}</h2>
            <div className="summary-line">
              <span>{t(msg`Meals`)}</span>
              <span>
                {money(
                  o.snapshot.quote.subtotal + o.snapshot.quote.premium,
                  o.market,
                  locale,
                )}
              </span>
            </div>
            <div className="summary-line">
              <span>{t(msg`Delivery fee`)}</span>
              <span>{money(o.snapshot.quote.shipping, o.market, locale)}</span>
            </div>
            <div className="summary-line summary-total">
              <span>{t(msg`Total`)}</span>
              <span>{money(o.total, o.market, locale)}</span>
            </div>
            {o.refunded > 0 && (
              <div className="summary-line">
                <span>{t(msg`Refund`)}</span>
                <span>{money(o.refunded, o.market, locale)}</span>
              </div>
            )}
            <p className="text-[10px] mt-5 text-muted-foreground break-all">
              {t(msg`Order reference`)}: {o.id}
            </p>
          </Card>
        </div>
      </div>
      <Dialog
        open={!!dialog}
        onOpenChange={(open) => {
          if (!open) setDialog(null);
        }}
      >
        <DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-lg">
          <DialogTitle>
            {dialog === "retry"
              ? t(msg`Review your payment`)
              : dialog === "cancel"
                ? t(msg`Cancel this box?`)
                : dialog === "edit"
                  ? t(msg`Make room for a new favorite.`)
                  : t(msg`Let us make it right.`)}
          </DialogTitle>
          <DialogDescription>
            {dialog === "retry"
              ? t(
                  msg`Check the current total. We'll confirm availability when you pay.`,
                )
              : dialog === "cancel"
                ? t(
                    msg`This box will be canceled and any payment refunded. Your weekly plan will continue.`,
                  )
                : dialog === "edit"
                  ? t(
                      msg`Swap your meals and review the updated total before saving.`,
                    )
                  : t(
                      msg`Tell us what happened. Your request and our response will appear in your account.`,
                    )}
          </DialogDescription>
          {dialog === "retry" ? (
            <>
              {retryQuote.error ? (
                <ErrorState
                  error={retryQuote.error}
                  retry={retryQuote.refresh}
                />
              ) : !retryQuote.data ? (
                <Loading />
              ) : (
                <p className="font-semibold my-4">
                  {t(msg`Total due`)}:{" "}
                  {money(retryQuote.data.total, o.market, locale)}
                </p>
              )}
              <Button
                disabled={busy || !retryQuote.data}
                onClick={() =>
                  perform(() => api.payOrder({ id, quote: retryQuote.data! }))
                }
              >
                {t(msg`Confirm payment`)}
              </Button>
            </>
          ) : dialog === "cancel" ? (
            <Button
              disabled={busy}
              onClick={() => perform(() => api.cancelOrder({ id }))}
            >
              {t(msg`Confirm cancellation`)}
            </Button>
          ) : dialog === "edit" ? (
            <>
              <div className="space-y-3">
                {(editMenu.data?.recipes ?? []).map((r) => (
                  <Label key={r.id} className="flex items-center gap-3 text-sm">
                    <Checkbox
                      checked={selected.includes(r.id)}
                      onCheckedChange={(e) =>
                        setSelected(
                          e
                            ? [...selected, r.id]
                            : selected.filter((x) => x !== r.id),
                        )
                      }
                    />
                    {recipeText(r, locale).name}{" "}
                    {r.premium > 0 && (
                      <span className="text-xs text-muted-foreground">
                        +
                        {money(
                          r.premium * o.snapshot.cart.servings,
                          o.market,
                          locale,
                        )}
                      </span>
                    )}
                  </Label>
                ))}
              </div>
              <p>
                {t(msg`Updated total`)}:{" "}
                {editQuote.data
                  ? money(editQuote.data.total, o.market, locale)
                  : "—"}
              </p>
              {editMenu.error && (
                <ErrorState error={editMenu.error} retry={editMenu.refresh} />
              )}
              {editQuote.error && (
                <ErrorState error={editQuote.error} retry={editQuote.refresh} />
              )}
              <Button
                disabled={
                  busy ||
                  !editQuote.data ||
                  selected.length !== o.snapshot.cart.mealCount
                }
                onClick={() =>
                  perform(() =>
                    api.editOrder({
                      id,
                      recipeIds: selected,
                      quote: editQuote.data!,
                    }),
                  )
                }
              >
                {t(msg`Confirm meals and price`)}
              </Button>
            </>
          ) : (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                perform(async () => {
                  await api.reportIssue({ orderId: id, category, message });
                  notice(
                    t(
                      msg`Request sent. You can follow our reply in your account.`,
                    ),
                  );
                  setMessage("");
                });
              }}
            >
              <ChoiceGroup
                columns
                label={t(msg`What went wrong?`)}
                value={category}
                onChange={setCategory}
                options={[
                  { value: "missing", label: t(msg`Missing item`) },
                  { value: "quality", label: t(msg`Ingredient quality`) },
                  { value: "delivery", label: t(msg`Delivery problem`) },
                  { value: "payment", label: t(msg`Payment question`) },
                ]}
              />
              <Field
                label={t(msg`Details`)}
                hint={t(
                  msg`Tell us which meal or delivery was affected and what happened.`,
                )}
              >
                <Textarea
                  required
                  minLength={10}
                  maxLength={2000}
                  value={message}
                  onChange={(e) => setMessage(e.target.value)}
                />
              </Field>
              <Button type="submit" disabled={busy}>
                {t(msg`Send request`)}
              </Button>
            </form>
          )}
        </DialogContent>
      </Dialog>
    </section>
  );
}
