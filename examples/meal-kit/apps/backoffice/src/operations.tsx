import {
  Collapsible,
  CollapsibleTrigger,
  CollapsibleContent,
} from "@gather/meal-kit/components/ui/collapsible";
import { SelectItem } from "@gather/meal-kit/components/select-field";
import { Tabs, TabsList, TabsTrigger } from "@gather/meal-kit/components/ui/tabs";
import { AnimatedTabsContent as TabsContent } from "@gather/meal-kit/components/animated-tabs-content";
import { RecipeFeedbackWorkspace } from "./components/feedback-workspace";
import { StaffTeam } from "./components/staff-team";
import { allows, canUseWorkspace, type StaffPermission } from "@gather/meal-kit/staff-domain";
import { Link } from "react-router-dom";

import { Card } from "@gather/meal-kit/components/ui/card";
import {
  Table,
  TableHeader,
  TableBody,
  TableRow,
  TableHead,
  TableCell,
} from "@gather/meal-kit/components/ui/table";
import { Label } from "@gather/meal-kit/components/ui/label";
import { Textarea } from "@gather/meal-kit/components/ui/textarea";
import { Checkbox } from "@gather/meal-kit/components/ui/checkbox";
import { CatalogWorkspace } from "./catalog-workspace";
import { recipeText } from "@gather/meal-kit/catalog-domain";
import { useAddressArea } from "@gather/meal-kit/components/delivery-address";
import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import { useState } from "react";
import { useGather } from "./state";
import * as api from "./api";
import { markets, money, deliveryLabel, deliveryDates } from "@gather/meal-kit/catalog";
import { transitions } from "@gather/meal-kit/domain";
import { SignIn } from "./auth-ui";
import { statusLabel } from "@gather/meal-kit/order-status";
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

export function Operations() {
  const { session, market, locale } = useGather();
  const { _: t } = useLingui();
  return (
    <SignIn>
      {session?.staff ? (
        canUseWorkspace(session.staff, market) ? (
          <Workspace />
        ) : (
          <section className="section">
            <Empty
              title={t(msg`Choose a country for your workspace`)}
              text={t(
                msg`Your staff access is available in the countries below.`,
              )}
            />
            <div className="flex flex-wrap gap-4">
              {session.staff.grants.map((grant) => (
                <Link
                  key={grant.market}
                  className="underline"
                  to={`/m/${grant.market}/${locale}/operations`}
                >
                  {t(markets[grant.market].name)}
                </Link>
              ))}
            </div>
          </section>
        )
      ) : (
        <Empty
          title={t(msg`Staff workspace`)}
          text={t(
            msg`This area is available to authorized operators. Sign in with your staff account.`,
          )}
        />
      )}
    </SignIn>
  );
}
function Workspace() {
  const { market, locale, act, busy, notice, session } = useGather();
  const { _: t } = useLingui();
  const result = useLoad(
    async () => api.getOperations({ market }),
    [market, JSON.stringify(session?.staff)],
  );
  const [tab, setTab] = useState("orders");
  const addressArea = useAddressArea();
  const [menuDate, setMenuDate] = useState(deliveryDates(market)[0]);
  const [issueId, setIssueId] = useState<string | null>(null);
  const [resolution, setResolution] = useState("");
  const [refund, setRefund] = useState("0");
  const [inventory, setInventory] = useState<{
    stock_key: string;
    available: number;
    published: boolean;
  } | null>(null);
  if (result.error)
    return <ErrorState error={result.error} retry={result.refresh} />;
  if (!result.data) return <Loading />;
  const { orders, stock, cases, recipeNames, access } = result.data;
  const can = (permission: StaffPermission) =>
    allows(access, permission, market);
  const sections = [
    {
      value: "orders",
      label: t(msg`Orders & fulfillment`),
      visible: can("orders"),
    },
    {
      value: "catalog",
      label: t(msg`Menus & recipes`),
      visible: can("catalog") || access.recipeEditor,
    },
    {
      value: "inventory",
      label: t(msg`Inventory & menu`),
      visible: can("inventory"),
    },
    {
      value: "support",
      label: t(msg`Support & refunds`),
      visible: can("support"),
    },
    {
      value: "feedback",
      label: t(msg`Recipe feedback`),
      visible: can("feedback"),
    },
    {
      value: "team",
      label: t(msg`Team access`),
      visible: access.administrator,
    },
  ].filter((section) => section.visible);
  const activeTab = sections.some((section) => section.value === tab)
    ? tab
    : sections[0]?.value;
  const perform = (fn: () => unknown | Promise<unknown>) =>
    act(async () => {
      await fn();
      setIssueId(null);
      setInventory(null);
      result.refresh();
    });
  function downloadPacking() {
    const rows = orders
      .filter(
        (o) =>
          ["packing", "packed"].includes(o.fulfillment) &&
          o.status !== "canceled",
      )
      .flatMap((o) =>
        o.snapshot.recipes.map((r) => [
          o.id,
          o.snapshot.cart.deliveryDate,
          o.snapshot.address.name,
          o.snapshot.address.country,
          addressArea(o.snapshot.address),
          o.snapshot.address.line,
          o.snapshot.address.phone,
          recipeText(r, locale).name,
          String(o.snapshot.cart.servings),
        ]),
      );
    const csv = [
      [
        "order",
        "delivery",
        "recipient",
        "country",
        "area",
        "street",
        "phone",
        "recipe",
        "servings",
      ],
      ...rows,
    ]
      .map((row) =>
        row
          .map(
            (cell) =>
              '"' + cell.replaceAll('"', '""').replace(/^[=+@-]/, "'") + '"',
          )
          .join(","),
      )
      .join("\r\n");
    const url = URL.createObjectURL(new Blob([csv], { type: "text/csv" }));
    const a = document.createElement("a");
    a.href = url;
    a.download = `gather-packing-${market}.csv`;
    a.click();
    URL.revokeObjectURL(url);
  }
  return (
    <section className="section">
      <SectionTitle
        eyebrow={t(msg`GATHER OPERATIONS`)}
        title={t(msg`Good food, thoughtfully delivered.`)}
        body={`${t(markets[market].region)} · ${t(msg`Demo fulfillment workspace`)}`}
      >
        {can("fulfillment") && (
          <Button variant="outline" onClick={downloadPacking}>
            {t(msg`Export packing list`)}
          </Button>
        )}
      </SectionTitle>
      {can("preview") && (
        <PaymentPreviewTools administrator={access.administrator} />
      )}
      {can("orders") && (
        <div className="ops-grid">
          {[
            [
              orders.filter((o) => o.status === "confirmed").length,
              t(msg`Confirmed boxes`),
            ],
            [
              orders.filter((o) => o.fulfillment === "packing").length,
              t(msg`In preparation`),
            ],
            [
              orders.filter((o) => o.fulfillment === "dispatched").length,
              t(msg`On the road`),
            ],
            [
              cases.filter((c) => c.status === "open").length,
              t(msg`Open support cases`),
            ],
          ].map(([n, l]) => (
            <div className="stat" key={l}>
              <strong>{n}</strong>
              <span>{l}</span>
            </div>
          ))}
        </div>
      )}
      <Tabs value={activeTab} onValueChange={(value) => setTab(String(value))}>
        <TabsList
          variant="line"
          className="account-tabs"
          aria-label={t(msg`Operations sections`)}
        >
          {sections.map(({ value, label }) => (
            <TabsTrigger key={value} value={value}>
              {label}
            </TabsTrigger>
          ))}
        </TabsList>
        <TabsContent value={activeTab}>
          {activeTab === "orders" ? (
            orders.length ? (
              <div className="table-wrap">
                <Table>
                  <TableHeader>
                    <TableRow>
                      {[
                        t(msg`Customer`),
                        t(msg`Delivery`),
                        t(msg`Box`),
                        t(msg`Payment`),
                        t(msg`Fulfillment`),
                        t(msg`Next action`),
                      ].map((l) => (
                        <TableHead key={l}>{l}</TableHead>
                      ))}
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {orders.map((o) => (
                      <TableRow key={o.id}>
                        <TableCell>
                          <strong>{o.snapshot.address.name}</strong>
                          <div className="text-[10px] text-muted-foreground">
                            {o.id}
                          </div>
                        </TableCell>
                        <TableCell>
                          {deliveryLabel(
                            o.snapshot.cart.deliveryDate,
                            o.market,
                            locale,
                          )}
                          <div className="text-[10px]">
                            {addressArea(o.snapshot.address)}
                          </div>
                        </TableCell>
                        <TableCell>
                          {o.snapshot.cart.mealCount} ×{" "}
                          {o.snapshot.cart.servings}
                          <div>{money(o.total, o.market, locale)}</div>
                        </TableCell>
                        <TableCell>
                          <Badge>{statusLabel(o.payment, t)}</Badge>
                          {o.refunded > 0 && (
                            <div>
                              {t(msg`Refunded`)}:{" "}
                              {money(o.refunded, o.market, locale)}
                            </div>
                          )}
                        </TableCell>
                        <TableCell>
                          {statusLabel(
                            o.status === "canceled"
                              ? "canceled"
                              : o.fulfillment,
                            t,
                          )}
                        </TableCell>
                        <TableCell>
                          <div className="flex gap-2">
                            {can("refund") &&
                              o.status === "payment_recovery" && (
                                <Button
                                  size="sm"
                                  disabled={busy}
                                  onClick={() =>
                                    perform(() =>
                                      api.refundRecoveredPayment({ id: o.id }),
                                    )
                                  }
                                >
                                  {t(msg`Refund late payment`)}
                                </Button>
                              )}
                            {can("preview") &&
                              ["processing", "requires_action"].includes(
                                o.payment,
                              ) &&
                              o.snapshot.paymentAttemptId && (
                                <>
                                  <Button
                                    size="sm"
                                    variant="outline"
                                    disabled={busy}
                                    onClick={() =>
                                      perform(() =>
                                        api.settleDemoPayment({
                                          attemptId:
                                            o.snapshot.paymentAttemptId!,
                                          outcome: "succeeded",
                                        }),
                                      )
                                    }
                                  >
                                    {t(msg`Simulate payment confirmation`)}
                                  </Button>
                                  <Button
                                    size="sm"
                                    variant="outline"
                                    disabled={busy}
                                    onClick={() =>
                                      perform(() =>
                                        api.settleDemoPayment({
                                          attemptId:
                                            o.snapshot.paymentAttemptId!,
                                          outcome: "failed",
                                        }),
                                      )
                                    }
                                  >
                                    {t(msg`Simulate payment decline`)}
                                  </Button>
                                  {o.status === "pending_payment" && (
                                    <Button
                                      size="sm"
                                      variant="outline"
                                      disabled={busy}
                                      onClick={() =>
                                        perform(() =>
                                          api.expireDemoCheckout({
                                            attemptId:
                                              o.snapshot.paymentAttemptId!,
                                          }),
                                        )
                                      }
                                    >
                                      {t(msg`Simulate checkout expiry`)}
                                    </Button>
                                  )}
                                </>
                              )}
                            {can("fulfillment") &&
                              o.payment === "succeeded" &&
                              o.status === "confirmed" &&
                              (transitions[o.fulfillment] ?? []).map((next) => (
                                <Button
                                  key={next}
                                  variant="outline"
                                  size="sm"
                                  disabled={busy}
                                  onClick={() =>
                                    perform(() =>
                                      api.advanceOrder({
                                        id: o.id,
                                        next: next as Parameters<
                                          typeof api.advanceOrder
                                        >[0]["next"],
                                      }),
                                    )
                                  }
                                >
                                  {statusLabel(next, t)}
                                </Button>
                              ))}
                          </div>
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </div>
            ) : (
              <Empty
                title={t(msg`Ready for the first box.`)}
                text={t(
                  msg`Place a demo order from the storefront to see it here.`,
                )}
              />
            )
          ) : activeTab === "catalog" ? (
            <CatalogWorkspace />
          ) : activeTab === "inventory" ? (
            <>
              <form
                className="flex gap-3 items-end mb-5"
                onSubmit={(e) => {
                  e.preventDefault();
                  perform(() => api.prepareMenu({ market, date: menuDate }));
                }}
              >
                <Field label={t(msg`Delivery date`)}>
                  <Select
                    value={menuDate}
                    onValueChange={(selectedValue) =>
                      setMenuDate(selectedValue)
                    }
                  >
                    {deliveryDates(market).map((date) => (
                      <SelectItem value={date} key={date}>
                        {deliveryLabel(date, market, locale)}
                      </SelectItem>
                    ))}
                  </Select>
                </Field>
                <Button
                  type="submit"
                  variant="outline"
                  className="mb-[18px]"
                  disabled={busy}
                >
                  {t(msg`Prepare menu inventory`)}
                </Button>
              </form>
              <p className="notice">
                {t(
                  msg`Set remaining servings and delivery capacity for each published menu before taking orders. The delivery row counts boxes.`,
                )}
              </p>
              <div className="table-wrap">
                <Table>
                  <TableHeader>
                    <TableRow>
                      {[
                        t(msg`Item`),
                        t(msg`Date`),
                        t(msg`Available`),
                        t(msg`Published`),
                        t(msg`Action`),
                      ].map((l) => (
                        <TableHead key={l}>{l}</TableHead>
                      ))}
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {stock.map((s) => (
                      <TableRow key={s.id}>
                        <TableCell>
                          {s.recipe_id === "delivery"
                            ? t(msg`Delivery capacity`)
                            : (recipeNames.find((r) => r.id === s.recipe_id)?.[
                                locale
                              ] ?? s.recipe_id)}
                        </TableCell>
                        <TableCell>{s.stock_key.split(":")[1]}</TableCell>
                        <TableCell>{s.available}</TableCell>
                        <TableCell>
                          {s.published ? t(msg`Yes`) : t(msg`No`)}
                        </TableCell>
                        <TableCell>
                          <Button
                            variant="outline"
                            size="sm"
                            onClick={() => setInventory(s)}
                          >
                            {t(msg`Edit`)}
                          </Button>
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </div>
            </>
          ) : activeTab === "team" ? (
            <StaffTeam />
          ) : activeTab === "feedback" ? (
            <RecipeFeedbackWorkspace />
          ) : cases.length ? (
            cases.map((c) => (
              <Card className="panel mb-4" key={c.id}>
                <div className="flex justify-between gap-4">
                  <Badge>{statusLabel(c.status, t)}</Badge>
                  <span className="text-xs">
                    {c.category} · {c.order_id}
                  </span>
                </div>
                <p className="my-4">{c.message}</p>
                {c.resolution ? (
                  <p className="notice">{c.resolution}</p>
                ) : (
                  <Button
                    variant="outline"
                    onClick={() => {
                      setIssueId(c.id);
                      setResolution("");
                      setRefund("0");
                    }}
                  >
                    {t(msg`Resolve request`)}
                  </Button>
                )}
              </Card>
            ))
          ) : (
            <Empty
              title={t(msg`Nothing waiting on you.`)}
              text={t(msg`Customer support requests will appear here.`)}
            />
          )}
        </TabsContent>
      </Tabs>
      <Dialog
        open={!!issueId}
        onOpenChange={(open) => {
          if (!open) setIssueId(null);
        }}
      >
        <DialogContent>
          <DialogTitle>{t(msg`Resolve support request`)}</DialogTitle>
          <DialogDescription>
            {t(
              msg`Record a response and an optional simulated refund. Refunds cannot exceed the remaining paid balance.`,
            )}
          </DialogDescription>
          <form
            onSubmit={(e) => {
              e.preventDefault();
              if (issueId)
                perform(() =>
                  api.resolveIssue({
                    id: issueId,
                    resolution,
                    refund: Math.round(Number(refund) * 100),
                    requestKey: crypto.randomUUID(),
                  }),
                );
            }}
          >
            <Field label={t(msg`Response`)}>
              <Textarea
                required
                minLength={5}
                value={resolution}
                onChange={(e) => setResolution(e.target.value)}
              />
            </Field>
            {can("refund") && (
              <Field
                label={`${t(msg`Refund amount`)} (${markets[market].currency})`}
              >
                <Input
                  type="number"
                  min="0"
                  step="0.01"
                  required
                  value={refund}
                  onChange={(e) => setRefund(e.target.value)}
                />
              </Field>
            )}
            <Button type="submit" disabled={busy}>
              {t(msg`Save resolution`)}
            </Button>
          </form>
        </DialogContent>
      </Dialog>
      <Dialog
        open={!!inventory}
        onOpenChange={(open) => {
          if (!open) setInventory(null);
        }}
      >
        <DialogContent>
          <DialogTitle>{t(msg`Edit available inventory`)}</DialogTitle>
          <DialogDescription>{inventory?.stock_key}</DialogDescription>
          {inventory && (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                perform(() =>
                  api.setInventory({
                    stockKey: inventory.stock_key,
                    available: inventory.available,
                    published: inventory.published,
                  }),
                );
              }}
            >
              <Field label={t(msg`Remaining quantity`)}>
                <Input
                  required
                  type="number"
                  min="0"
                  max="10000"
                  value={inventory.available}
                  onChange={(e) =>
                    setInventory({
                      ...inventory,
                      available: Number(e.target.value),
                    })
                  }
                />
              </Field>
              <Label className="flex gap-3 mb-5">
                <Checkbox
                  checked={inventory.published}
                  onCheckedChange={(e) =>
                    setInventory({ ...inventory, published: e })
                  }
                />
                {t(msg`Available to order`)}
              </Label>
              <Button type="submit" disabled={busy}>
                {t(msg`Save inventory`)}
              </Button>
            </form>
          )}
        </DialogContent>
      </Dialog>
    </section>
  );
}

function PaymentPreviewTools({ administrator }: { administrator: boolean }) {
  const { _: t } = useLingui();
  const { market, act, busy, notice } = useGather();
  const [customerId, setCustomerId] = useState("");
  const [outcome, setOutcome] = useState<
    "succeeded" | "failed" | "requires_action" | "processing"
  >("succeeded");
  return (
    <Collapsible className="panel mb-6">
      <CollapsibleTrigger
        className="cursor-pointer font-medium"
        render={<Button variant="ghost" className="px-0" />}
      >
        {t(msg`Preview tools`)}
      </CollapsibleTrigger>
      <CollapsibleContent>
        {administrator && (
          <Button
            variant="outline"
            className="mt-4"
            disabled={busy}
            onClick={() =>
              act(async () => {
                await api.loadSampleMenus({ market });
                notice(
                  t(msg`Sample menus are ready. Reload the menu to browse.`),
                );
              })
            }
          >
            {t(msg`Load sample menus for this country`)}
          </Button>
        )}
        <form
          className="mt-5"
          onSubmit={(event) => {
            event.preventDefault();
            void act(async () => {
              await api.setPaymentScenario({ customerId, market, outcome });
              notice(t(msg`The next checkout will use this payment scenario.`));
            });
          }}
        >
          <p className="text-sm text-muted-foreground mb-4">
            {t(
              msg`Staff only. Set the payment result for a customer's next checkout in this market. No real payment is taken.`,
            )}
          </p>
          <Field label={t(msg`Customer account ID`)}>
            <Input
              required
              value={customerId}
              onChange={(event) => setCustomerId(event.target.value)}
            />
          </Field>
          <Field label={t(msg`Payment scenario`)}>
            <Select
              value={outcome}
              onValueChange={(selectedValue) =>
                setOutcome(selectedValue as typeof outcome)
              }
            >
              <SelectItem value="succeeded">
                {t(msg`Payment succeeds`)}
              </SelectItem>
              <SelectItem value="failed">{t(msg`Payment declined`)}</SelectItem>
              <SelectItem value="processing">
                {t(msg`Payment confirmation delayed`)}
              </SelectItem>
              <SelectItem value="requires_action">
                {t(msg`Additional authentication required`)}
              </SelectItem>
            </Select>
          </Field>
          <Button type="submit" disabled={busy}>
            {t(msg`Set next checkout`)}
          </Button>
        </form>
      </CollapsibleContent>
    </Collapsible>
  );
}
