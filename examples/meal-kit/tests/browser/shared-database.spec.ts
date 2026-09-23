// The one test the two apps were merged onto a single database to make
// possible: a person working in the back office interface changes an order,
// and the customer sees the change in the storefront interface. Both halves
// are driven through the real UI, and each browser talks only to its own app.

import { test, expect } from "./fixtures";
import { signIn, rpc, subject, visit } from "./helpers";
import { testOrigin, backofficeOrigin } from "../fixture/settings";
import { defaultCart, type Order, type Quote } from "@gather/meal-kit/domain";
import { deliveryDates } from "@gather/meal-kit/catalog";

test("an operator advancing a box in the back office interface changes what the customer sees in the storefront", async ({
  page,
  context,
  browser,
}) => {
  const ops = await browser.newContext({ baseURL: backofficeOrigin });
  try {
    await signIn(page, "alex@gather.example", false);
    const opsPage = await ops.newPage();
    await signIn(opsPage, "ops@gather.example");
    // State this customer's payment outcome instead of inheriting one. The
    // demo scenario is per customer and outlives the spec that armed it, so a
    // spec that only reads it is reading whatever ran before.
    await rpc(ops, "paymentScenario", {
      customerId: await subject(context),
      market: "us",
      outcome: "succeeded",
    });

    // Every request either browser makes, so the closing assertion can say
    // whether the state crossed through the database or through a call. The
    // own-origin tallies are what keep "asked nobody" from passing over a
    // listener that was never wired.
    const crossings: string[] = [];
    const asked = { storefront: 0, backoffice: 0 };
    page.on("request", (request) => {
      if (request.url().startsWith(backofficeOrigin))
        crossings.push(`storefront page asked the back office for ${request.url()}`);
      if (request.url().startsWith(testOrigin)) asked.storefront++;
    });
    opsPage.on("request", (request) => {
      if (request.url().startsWith(testOrigin))
        crossings.push(`back office page asked the storefront for ${request.url()}`);
      if (request.url().startsWith(backofficeOrigin)) asked.backoffice++;
    });

    // Setup, not the subject: the customer's own app places the box. The
    // subject is what the two interfaces show each other afterwards.
    const cart = {
      ...defaultCart("us"),
      deliveryDate: deliveryDates("us").at(-1)!,
      postal: "10001",
      recurring: false,
      mealCount: 2 as const,
      recipeIds: ["lemon-chicken", "pesto-pasta"],
    };
    const order = await rpc<Order>(context, "checkout", {
      cart,
      quote: await rpc<Quote>(context, "quote", cart),
      requestKey: crypto.randomUUID(),
      consent: true,
      address: {
        country: "US", province: "NY", district: "", city: "New York",
        name: "Shared database customer", email: "alex@gather.example",
        line: "9 Garden Street", postal: cart.postal,
        phone: "+12125550123", instructions: "",
      },
    });
    expect(order).toMatchObject({ status: "confirmed", payment: "succeeded", fulfillment: "unallocated" });

    // The storefront, before anyone touches the box.
    await visit(page, `/m/us/en/orders/${order.id}`);
    const customerStatus = page.locator(".checkout-grid .panel").first();
    await expect(customerStatus.getByText("Awaiting preparation", { exact: true })).toBeVisible();

    // The back office interface: the box the storefront wrote is on the board,
    // reached by no call to the storefront - only by binding its database.
    await visit(opsPage, "/m/us/en/operations");
    const row = opsPage.getByRole("row").filter({ hasText: order.id });
    await expect(row).toContainText("Shared database customer");
    await expect(row).toContainText("Awaiting preparation");

    // The change, made the way an operator makes it.
    await row.getByRole("button", { name: "Packing", exact: true }).click();
    await expect(row.getByRole("button", { name: "Ready for collection", exact: true })).toBeVisible();
    await expect(row.getByRole("button", { name: "Packing", exact: true })).toHaveCount(0);
    await expect(row.getByText("Packing", { exact: true })).toBeVisible();

    // The customer's page still shows the state it loaded; the new one arrives
    // when the storefront next reads the database.
    await expect(customerStatus.getByText("Awaiting preparation", { exact: true })).toBeVisible();
    await page.reload();
    await expect(customerStatus.getByText("Packing", { exact: true })).toBeVisible();
    await expect(customerStatus.getByText("Awaiting preparation", { exact: true })).toHaveCount(0);

    expect(asked.storefront).toBeGreaterThan(0);
    expect(asked.backoffice).toBeGreaterThan(0);
    expect(crossings).toEqual([]);
  } finally {
    await ops.close();
  }
});
