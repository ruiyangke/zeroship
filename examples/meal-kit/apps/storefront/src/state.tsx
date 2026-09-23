import { Alert, AlertDescription } from "@gather/meal-kit/components/ui/alert";
import { Button } from "@gather/meal-kit/components/ui/button";
import { X } from "lucide-react";

import { msg } from "@lingui/core/macro";
import { useLingui } from "@lingui/react";
import {
  createContext,
  useContext,
  useEffect,
  useRef,
  useState,
  useSyncExternalStore,
  type ReactNode,
} from "react";
import { createAuthClient } from "@zeroship/auth/client";
import { useNavigate, useParams } from "react-router-dom";
import * as api from "./api";
import { type Cart } from "@gather/meal-kit/domain";
import { markets, type Locale, type MarketId } from "@gather/meal-kit/catalog";
import { DraftController, type DraftSnapshot } from "./draft-controller";
import { draftTransport } from "./draft-transport";
import { cookingTimers } from "./cooking-timers";
import { preferencesSchema, type Preferences } from "@gather/meal-kit/account-domain";
import type { CookingUnits } from "@gather/meal-kit/cooking-domain";

type Session = Awaited<ReturnType<typeof api.getSession>>;
type State = {
  locale: Locale;
  market: MarketId;
  cart: Cart;
  catalog: Awaited<ReturnType<typeof api.getCatalog>> | undefined;
  catalogError: string;
  refreshCatalog: () => void;
  setCart: (next: Cart | ((old: Cart) => Cart)) => void;
  draft: DraftSnapshot;
  retryDraft: () => Promise<void>;
  flushDraft: () => Promise<boolean>;
  resolveDraft: (source: "local" | "saved") => Promise<void>;
  session: Session | null;
  sessionError: string;
  favorites: string[];
  preferredUnits: CookingUnits;
  savePreferences: (patch: Partial<Preferences>) => Promise<void>;
  toggleFavorite: (id: string) => Promise<void>;
  refreshSession: () => Promise<void>;
  login: () => Promise<void>;
  logout: () => Promise<void>;
  path: (route?: string) => string;
  notice: (message: string) => void;
  busy: boolean;
  act: <T>(fn: () => Promise<T>) => Promise<T | undefined>;
};
const Context = createContext<State | null>(null);
export const useGather = () => {
  const ctx = useContext(Context);
  if (!ctx) throw new Error("Gather context is missing");
  return ctx;
};
let authClient: ReturnType<typeof createAuthClient> | undefined;
function client() {
  return (authClient ??= createAuthClient());
}

export function GatherProvider({ children }: { children: ReactNode }) {
  const { _: t, i18n } = useLingui();
  const params = useParams();
  const navigate = useNavigate();
  const market: MarketId =
    params.market && params.market in markets
      ? (params.market as MarketId)
      : "us";
  const locale = i18n.locale as Locale;
  const [draftController] = useState(
    () => new DraftController(market, draftTransport(market)),
  );
  const draft = useSyncExternalStore(
    draftController.subscribe,
    draftController.getSnapshot,
  );
  const { cart } = draft;
  const setCart = draftController.setCart;
  const [catalog, setCatalog] =
    useState<Awaited<ReturnType<typeof api.getCatalog>>>();
  const [catalogError, setCatalogError] = useState("");
  const [catalogReload, setCatalogReload] = useState(0);
  useEffect(() => {
    let live = true;
    setCatalog(undefined);
    setCatalogError("");
    Promise.resolve(api.getCatalog({ market, date: cart.deliveryDate }))
      .then((data) => {
        if (live) setCatalog(data);
      })
      .catch((error) => {
        if (live) setCatalogError(String(error.message ?? error));
      });
    return () => {
      live = false;
    };
  }, [market, cart.deliveryDate, catalogReload]);
  const [session, setSession] = useState<Session | null>(null);
  const [sessionError, setSessionError] = useState("");
  const [favorites, setFavorites] = useState<string[]>([]);
  const [preferredUnits, setPreferredUnits] = useState<CookingUnits>("metric");
  const preferencesRevision = useRef(0);
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState(false);
  const refreshSession = async () => {
    setSessionError("");
    const next = await api.getSession();
    cookingTimers.setOwner(next.user?.id ?? null);
    setSession(next);
    await draftController.activate(next.user?.id ?? null);
  };
  useEffect(() => {
    refreshSession().catch((error) =>
      setSessionError(String(error.message ?? error)),
    );
  }, []);
  useEffect(() => {
    const focus = () => {
      void refreshSession()
        .then(() => draftController.refresh())
        .catch(() => {});
    };
    const unload = (event: BeforeUnloadEvent) => {
      if (draftController.getSnapshot().dirty) {
        event.preventDefault();
        event.returnValue = "";
      }
    };
    window.addEventListener("focus", focus);
    window.addEventListener("beforeunload", unload);
    return () => {
      window.removeEventListener("focus", focus);
      window.removeEventListener("beforeunload", unload);
      void draftController.flush();
    };
  }, [draftController]);
  useEffect(() => {
    if (!session?.user) {
      setFavorites([]);
      setPreferredUnits("metric");
      return;
    }
    let live = true;
    const revision = preferencesRevision.current;
    Promise.resolve(api.getAccount({ market }))
      .then((account) => {
        const preferences = account.profile?.preferences as
          | { favorites?: string[] }
          | undefined;
        if (live && revision === preferencesRevision.current) {
          setFavorites(preferences?.favorites ?? []);
          setPreferredUnits(
            preferencesSchema.parse(account.profile?.preferences ?? {}).units,
          );
        }
      })
      .catch((error) => setMessage(String(error.message ?? error)));
    return () => {
      live = false;
    };
  }, [session?.user?.id]);
  async function savePreferences(patch: Partial<Preferences>) {
    const profile = await api.savePreferences(patch);
    const preferences = preferencesSchema.parse(profile.preferences);
    preferencesRevision.current += 1;
    setFavorites(preferences.favorites);
    setPreferredUnits(preferences.units);
  }
  async function toggleFavorite(id: string) {
    if (!session?.user) {
      setMessage(t(msg`Sign in to save your favorite recipes.`));
      return;
    }
    await act(async () => {
      const account = await api.getAccount({ market });
      const preferences = account.profile?.preferences as
        | { favorites?: string[]; exclude?: string[] }
        | undefined;
      const current = preferences?.favorites ?? [];
      const next = current.includes(id)
        ? current.filter((value) => value !== id)
        : [...current, id];
      await savePreferences({ favorites: next });
    });
  }
  useEffect(() => {
    if (!message) return;
    const timer = setTimeout(() => setMessage(""), 6000);
    return () => clearTimeout(timer);
  }, [message]);
  const path = (route = "") => `/m/${market}/${locale}${route}`;
  async function act<T>(fn: () => Promise<T>): Promise<T | undefined> {
    setBusy(true);
    try {
      return await fn();
    } catch (error) {
      setMessage(
        error instanceof Error
          ? error.message
          : t(msg`Something went wrong. Please try again.`),
      );
      return undefined;
    } finally {
      setBusy(false);
    }
  }
  async function login() {
    if (!(await draftController.prepareSignIn())) return;
    await client().signInWithOAuth({ provider: "password", popup: true });
    await refreshSession();
  }
  async function logout() {
    if (!(await draftController.prepareSignIn())) return;
    await client().signOut();
    await refreshSession();
    navigate(path());
  }
  return (
    <Context.Provider
      value={{
        locale,
        market,
        cart,
        catalog,
        catalogError,
        refreshCatalog: () => setCatalogReload((n) => n + 1),
        setCart,
        draft,
        retryDraft: () => draftController.retry(),
        flushDraft: draftController.flush,
        resolveDraft: (source) => draftController.choose(source),
        session,
        sessionError,
        favorites,
        preferredUnits,
        savePreferences,
        toggleFavorite,
        refreshSession,
        login,
        logout,
        path,
        notice: setMessage,
        busy,
        act,
      }}
    >
      {children}
      {message && (
        <Alert role="status" className="toast flex items-start gap-3">
          <AlertDescription className="text-inherit flex-1">
            {Object.hasOwn(i18n.messages, message) ? t(message) : message}
          </AlertDescription>
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label={t(msg`Close`)}
            onClick={() => setMessage("")}
          >
            <X />
          </Button>
        </Alert>
      )}
    </Context.Provider>
  );
}
