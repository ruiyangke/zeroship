import { describe, expect, test, vi } from "vitest";
import { DraftController, type DraftTransport } from "../src/draft-controller";
import { defaultCart } from "@gather/meal-kit/domain";
import type { DraftLoad, DraftSave, SavedDraft } from "@gather/meal-kit/draft-domain";

const cart = {
  ...defaultCart("us"),
  postal: "10001",
  recipeIds: ["lemon-chicken"],
};
const saved = (version: number, value = cart): SavedDraft => ({
  id: "meal_saved",
  version,
  cart: value,
});
const state = (draft: SavedDraft | null): DraftLoad => ({ draft, guest: null });
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((yes) => {
    resolve = yes;
  });
  return { promise, resolve };
}
function fixture() {
  const transport: DraftTransport = {
    load: vi.fn(async () => state(null)),
    save: vi.fn(async (_owner, next, expected) => ({
      saved: true,
      state: state(saved((expected?.version ?? 0) + 1, next)),
    })),
    attach: vi.fn(async () => ({ saved: false, state: state(null) })),
  };
  return { transport, controller: new DraftController("us", transport) };
}

describe("saved box synchronization", () => {
  test("editing during a refresh of the same revision does not invent a remote conflict", async () => {
    const { controller, transport } = fixture();
    vi.mocked(transport.load).mockResolvedValueOnce(state(saved(1)));
    await controller.activate("customer");
    const load = deferred<DraftLoad>();
    vi.mocked(transport.load).mockReturnValueOnce(load.promise);
    const refresh = controller.refresh();
    controller.setCart({ ...cart, servings: 4 });
    load.resolve(state(saved(1)));
    await refresh;
    await controller.flush();
    expect(controller.getSnapshot()).toMatchObject({ conflict: null, dirty: false, cart: { servings: 4 } });
    expect(transport.save).toHaveBeenCalledWith("customer", expect.objectContaining({ servings: 4 }), { id: "meal_saved", version: 1 }, expect.any(String));
  });
  test("a delayed refresh cannot replace a newer saved result", async () => {
    const { controller, transport } = fixture();
    vi.mocked(transport.load).mockResolvedValueOnce(state(saved(1)));
    await controller.activate("customer");
    const load = deferred<DraftLoad>();
    vi.mocked(transport.load).mockReturnValueOnce(load.promise);
    const refresh = controller.refresh();
    controller.setCart({ ...cart, servings: 4 });
    await controller.flush();
    load.resolve(state(saved(1)));
    await refresh;
    expect(controller.getSnapshot()).toMatchObject({ conflict: null, dirty: false, cart: { servings: 4 } });
    controller.setCart({ ...cart, servings: 6 });
    await controller.flush();
    expect(transport.save).toHaveBeenLastCalledWith("customer", expect.objectContaining({ servings: 6 }), { id: "meal_saved", version: 2 }, expect.any(String));
  });
  test("an untouched account box accepts the guest's choices without a conflict", async () => {
    const { controller, transport } = fixture();
    const account = saved(1, defaultCart("us"));
    const guest = { ...saved(1), id: "meal_guest" };
    vi.mocked(transport.load).mockResolvedValueOnce({ draft: account, guest });
    vi.mocked(transport.attach).mockResolvedValueOnce({
      saved: true,
      state: state(saved(2)),
    });
    await controller.activate("customer");
    expect(transport.attach).toHaveBeenCalledWith(
      "customer",
      "guest",
      { draft: account, guest },
      expect.any(String),
    );
    expect(controller.getSnapshot()).toMatchObject({ conflict: null, cart });
  });
  test("an empty saved box takes the edits made while the first load was in flight", async () => {
    const { controller, transport } = fixture();
    const load = deferred<DraftLoad>();
    vi.mocked(transport.load).mockReturnValueOnce(load.promise);
    const boot = controller.activate("customer");
    controller.setCart({ ...defaultCart("us"), postal: "10001" });
    load.resolve(state(saved(2, defaultCart("us"))));
    await boot;
    expect(controller.getSnapshot()).toMatchObject({
      conflict: null,
      ready: true,
      cart: { postal: "10001" },
    });
    await controller.flush();
    expect(transport.save).toHaveBeenCalledWith(
      "customer",
      expect.objectContaining({ postal: "10001" }),
      { id: "meal_saved", version: 2 },
      expect.any(String),
    );
    expect(controller.getSnapshot().dirty).toBe(false);
  });
  test("edits during a delayed load remain available for an explicit conflict choice", async () => {
    const { controller, transport } = fixture();
    const load = deferred<DraftLoad>();
    vi.mocked(transport.load).mockReturnValueOnce(load.promise);
    const boot = controller.activate(null);
    controller.setCart({ ...cart, servings: 4 });
    load.resolve(state(saved(2)));
    await boot;
    expect(controller.getSnapshot().cart.servings).toBe(4);
    expect(controller.getSnapshot().conflict?.draft?.version).toBe(2);
    expect(await controller.flush()).toBe(false);
    await controller.choose("local");
    expect(transport.save).toHaveBeenCalledWith(
      null,
      expect.objectContaining({ servings: 4 }),
      { id: "meal_saved", version: 2 },
      expect.any(String),
    );
    expect(controller.getSnapshot().dirty).toBe(false);
  });
  test("serializes rapid changes and retries an uncertain write with the same operation key", async () => {
    const { controller, transport } = fixture();
    await controller.activate(null);
    controller.setCart(cart);
    vi.mocked(transport.save).mockRejectedValueOnce(new Error("response lost"));
    expect(await controller.flush()).toBe(false);
    const first = vi.mocked(transport.save).mock.calls[0];
    controller.setCart({ ...cart, servings: 10 });
    const pending = deferred<DraftSave>();
    vi.mocked(transport.save).mockReturnValueOnce(pending.promise);
    const retry = controller.flush();
    expect(vi.mocked(transport.save).mock.calls[1]).toEqual(first);
    controller.setCart({ ...cart, servings: 3 });
    pending.resolve({ saved: true, state: state(saved(1)) });
    await retry;
    expect(transport.save).toHaveBeenLastCalledWith(
      null,
      expect.objectContaining({ servings: 3 }),
      { id: "meal_saved", version: 1 },
      expect.any(String),
    );
    expect(controller.getSnapshot()).toMatchObject({
      dirty: false,
      saving: false,
      error: "",
      cart: { servings: 3 },
    });
  });
  test("late results from a previous identity cannot populate another customer's box", async () => {
    const { controller, transport } = fixture();
    const old = deferred<DraftLoad>();
    vi.mocked(transport.load)
      .mockReturnValueOnce(old.promise)
      .mockResolvedValueOnce(state(saved(2, { ...cart, servings: 6 })));
    const boot = controller.activate("first-user");
    await controller.activate("second-user");
    old.resolve(state(saved(1)));
    await boot;
    expect(controller.getSnapshot().cart.servings).toBe(6);
    await controller.activate(null);
    expect(controller.getSnapshot().cart.recipeIds).toEqual([]);
  });
  test("different guest and account boxes require a choice and stale choices remain reviewable", async () => {
    const { controller, transport } = fixture();
    const account = saved(2, { ...cart, servings: 4 });
    const guest = { ...saved(1), id: "meal_guest" };
    vi.mocked(transport.load).mockResolvedValueOnce({ draft: account, guest });
    await controller.activate("customer");
    expect(transport.attach).not.toHaveBeenCalled();
    expect(controller.getSnapshot().conflict).toEqual({
      draft: account,
      guest,
    });
    const newer = { ...account, version: 3 };
    vi.mocked(transport.attach).mockResolvedValueOnce({
      saved: false,
      state: { draft: newer, guest },
    });
    await controller.choose("local");
    expect(controller.getSnapshot().conflict?.draft?.version).toBe(3);
    vi.mocked(transport.attach).mockResolvedValueOnce({
      saved: true,
      state: state(newer),
    });
    await controller.choose("saved");
    expect(controller.getSnapshot()).toMatchObject({
      conflict: null,
      cart: { servings: 4 },
    });
  });
});
