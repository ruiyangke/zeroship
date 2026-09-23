import { defaultCart, type Cart } from "@gather/meal-kit/domain";
import type { MarketId } from "@gather/meal-kit/catalog";
import {
  draftContents,
  draftHasChoices,
  draftRevision,
  type DraftLoad,
  type DraftRevision,
  type DraftSave,
} from "@gather/meal-kit/draft-domain";

export type DraftTransport = {
  load(owner: string | null): Promise<DraftLoad>;
  save(
    owner: string | null,
    cart: Cart,
    expected: DraftRevision | null,
    requestKey: string,
  ): Promise<DraftSave>;
  attach(
    owner: string,
    source: "guest" | "account",
    state: DraftLoad,
    requestKey: string,
  ): Promise<DraftSave>;
};
export type DraftSnapshot = {
  cart: Cart;
  ready: boolean;
  saving: boolean;
  dirty: boolean;
  error: string;
  conflict: DraftLoad | null;
};

export class DraftController {
  private owner: string | null | undefined;
  private generation = 0;
  private server: DraftLoad = { draft: null, guest: null };
  private snapshot: DraftSnapshot;
  private listeners = new Set<() => void>();
  private timer?: ReturnType<typeof setTimeout>;
  private loading?: Promise<void>;
  private writing?: Promise<boolean>;
  private pending?: { cart: Cart; expected: DraftRevision | null; key: string };
  constructor(
    readonly market: MarketId,
    private transport: DraftTransport,
  ) {
    this.snapshot = {
      cart: defaultCart(market),
      ready: false,
      saving: false,
      dirty: false,
      error: "",
      conflict: null,
    };
  }
  getSnapshot = () => this.snapshot;
  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };
  private update(next: Partial<DraftSnapshot>) {
    this.snapshot = { ...this.snapshot, ...next };
    this.listeners.forEach((listener) => listener());
  }
  activate(owner: string | null) {
    if (owner === this.owner) return this.loading ?? Promise.resolve();
    const first = this.owner === undefined;
    this.owner = owner;
    this.generation++;
    clearTimeout(this.timer);
    this.server = { draft: null, guest: null };
    this.pending = undefined;
    this.writing = undefined;
    this.update({
      ...(!first && { cart: defaultCart(this.market), dirty: false }),
      ready: false,
      saving: false,
      error: "",
      conflict: null,
    });
    return this.reload();
  }
  private reload() {
    const generation = this.generation;
    if (this.owner === undefined) return Promise.resolve();
    const owner = this.owner;
    const baseline = this.server;
    this.loading = (async () => {
      try {
        const state = await this.transport.load(owner);
        if (generation !== this.generation || this.server !== baseline) return;
        this.server = state;
        if (state.guest && owner) {
          if (
            !state.draft ||
            !draftHasChoices(state.draft.cart) ||
            draftContents(state.draft.cart) === draftContents(state.guest.cart)
          ) {
            const attached = await this.transport.attach(
              owner,
              state.draft && draftHasChoices(state.draft.cart)
                ? "account"
                : "guest",
              state,
              crypto.randomUUID(),
            );
            if (generation !== this.generation) return;
            this.server = attached.state;
            if (!attached.saved) {
              this.update({ ready: true, conflict: attached.state, error: "" });
              return;
            }
          } else {
            this.update({
              cart: state.guest.cart,
              ready: true,
              conflict: state,
              error: "",
            });
            return;
          }
        }
        const remote = this.server.draft;
        // A box with no choices in it is the same empty box the customer is
        // already looking at, so there is nothing to choose between and the
        // edits made while the load was in flight simply stand. The guest
        // branch above reads an untouched account box the same way. Without
        // this, the very first load raises a conflict against any edit made
        // before it arrives: `baseline.draft` is null until a load lands, so
        // "the saved revision moved" is true for every first load.
        if (
          this.snapshot.dirty &&
          remote &&
          draftHasChoices(remote.cart) &&
          JSON.stringify(draftRevision(remote)) !== JSON.stringify(draftRevision(baseline.draft)) &&
          draftContents(remote.cart) !== draftContents(this.snapshot.cart)
        ) {
          this.update({ ready: true, conflict: this.server, error: "" });
          return;
        }
        if (
          !this.snapshot.dirty ||
          (remote &&
            draftContents(remote.cart) === draftContents(this.snapshot.cart))
        )
          this.update({
            cart: remote?.cart ?? defaultCart(this.market),
            dirty: false,
          });
        this.update({ ready: true, error: "", conflict: null });
        if (this.snapshot.dirty) void this.flush();
      } catch {
        if (generation === this.generation)
          this.update({
            error:
              /* i18n */ "We couldn't load your saved box. Your choices on this page are still here.",
          });
      }
    })();
    return this.loading;
  }
  setCart = (next: Cart | ((cart: Cart) => Cart)) => {
    const cart = typeof next === "function" ? next(this.snapshot.cart) : next;
    if (cart.market !== this.market) throw new Error("Draft country mismatch");
    if (draftContents(cart) === draftContents(this.snapshot.cart)) return;
    this.update({ cart, dirty: true });
    clearTimeout(this.timer);
    this.timer = setTimeout(() => {
      void this.flush();
    }, 250);
  };
  flush = (): Promise<boolean> => {
    clearTimeout(this.timer);
    if (this.writing) return this.writing;
    if (
      !this.snapshot.ready ||
      this.owner === undefined ||
      this.snapshot.conflict
    )
      return Promise.resolve(false);
    const generation = this.generation;
    const owner = this.owner;
    this.writing = (async () => {
      while (this.snapshot.dirty && generation === this.generation) {
        this.pending ??= {
          cart: this.snapshot.cart,
          expected: draftRevision(this.server.draft),
          key: crypto.randomUUID(),
        };
        const pending = this.pending;
        this.update({ saving: true, error: "" });
        try {
          const result = await this.transport.save(
            owner,
            pending.cart,
            pending.expected,
            pending.key,
          );
          if (generation !== this.generation) return false;
          this.server = result.state;
          if (
            !result.saved &&
            (!result.state.draft ||
              result.state.guest ||
              draftContents(result.state.draft.cart) !==
                draftContents(this.snapshot.cart))
          ) {
            this.pending = undefined;
            this.update({ conflict: result.state, saving: false });
            return false;
          }
          this.pending = undefined;
          if (
            draftContents(this.snapshot.cart) === draftContents(pending.cart) ||
            (result.state.draft &&
              draftContents(this.snapshot.cart) ===
                draftContents(result.state.draft.cart))
          )
            this.update({ dirty: false });
        } catch {
          if (generation === this.generation)
            this.update({
              error:
                /* i18n */ "We couldn't save your box. Your choices are still here.",
              saving: false,
            });
          return false;
        }
      }
      if (generation === this.generation) this.update({ saving: false });
      return generation === this.generation;
    })().finally(() => {
      if (generation === this.generation) this.writing = undefined;
    });
    return this.writing;
  };
  async prepareSignIn() {
    await this.loading;
    return !this.snapshot.dirty || (await this.flush());
  }
  async refresh() {
    if (this.snapshot.saving || this.snapshot.conflict) return;
    if (this.snapshot.dirty) {
      await this.flush();
      return;
    }
    await this.reload();
  }
  async retry() {
    if (!this.snapshot.ready) await this.reload();
    else await this.flush();
  }
  async choose(source: "local" | "saved") {
    const conflict = this.snapshot.conflict;
    if (!conflict || this.owner === undefined) return;
    const generation = this.generation;
    if (conflict.guest && this.owner) {
      this.update({ saving: true, error: "" });
      try {
        const result = await this.transport.attach(
          this.owner,
          source === "local" ? "guest" : "account",
          conflict,
          crypto.randomUUID(),
        );
        if (generation !== this.generation) return;
        this.server = result.state;
        this.update({
          saving: false,
          conflict: result.saved ? null : result.state,
          ...(result.saved && {
            cart: result.state.draft?.cart ?? defaultCart(this.market),
            dirty: false,
          }),
        });
      } catch {
        if (generation === this.generation)
          this.update({
            saving: false,
            error:
              /* i18n */ "We couldn't save your box. Your choices are still here.",
          });
      }
      return;
    }
    this.server = conflict;
    this.pending = undefined;
    this.update({
      conflict: null,
      error: "",
      ...(source === "saved"
        ? {
            cart: conflict.draft?.cart ?? defaultCart(this.market),
            dirty: false,
          }
        : { dirty: true }),
    });
    if (source === "local") await this.flush();
  }
}
