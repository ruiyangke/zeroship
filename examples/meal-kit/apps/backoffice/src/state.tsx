import { createContext, useContext, useEffect, useState, type ReactNode } from "react";
import { createAuthClient } from "@zeroship/auth/client";
import { useParams } from "react-router-dom";
import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import { markets, type MarketId, type Locale } from "@gather/meal-kit/catalog";
import { Alert, AlertDescription } from "@gather/meal-kit/components/ui/alert";
import * as api from "./api";

type Session = Awaited<ReturnType<typeof api.getSession>>;
type State = {
  market: MarketId; locale: Locale; session: Session | null; sessionError: string;
  refreshSession: () => Promise<void>; login: () => Promise<void>; logout: () => Promise<void>;
  path: (route?: string) => string; busy: boolean; notice: (message: string) => void;
  act: <T>(action: () => Promise<T> | T) => Promise<T | undefined>;
};
const Context = createContext<State | null>(null);
export function useGather() {
  const value = useContext(Context);
  if (!value) throw new Error("Gather staff context is missing");
  return value;
}
let client: ReturnType<typeof createAuthClient> | undefined;
const auth = () => client ??= createAuthClient();
export function GatherProvider({ children }: { children: ReactNode }) {
  const params = useParams();
  const { i18n, _: t } = useLingui();
  const market = (params.market && Object.hasOwn(markets, params.market) ? params.market : "us") as MarketId;
  const locale = i18n.locale as Locale;
  const [session, setSession] = useState<Session | null>(null);
  const [sessionError, setSessionError] = useState("");
  const [message, notice] = useState("");
  const [busy, setBusy] = useState(false);
  async function refreshSession() {
    setSessionError("");
    try { setSession(await api.getSession()); }
    catch (error) { setSessionError(String((error as Error).message)); throw error; }
  }
  useEffect(() => {
    const refresh = () => { void refreshSession().catch(() => {}); };
    refresh(); window.addEventListener("focus", refresh);
    return () => window.removeEventListener("focus", refresh);
  }, []);
  useEffect(() => { if (!message) return; const timer = setTimeout(() => notice(""), 6000); return () => clearTimeout(timer); }, [message]);
  async function act<T>(action: () => Promise<T> | T): Promise<T | undefined> {
    setBusy(true);
    try { return await action(); }
    catch (error) { notice(error instanceof Error ? error.message : t(msg`Something went wrong. Please try again.`)); }
    finally { setBusy(false); }
  }
  return <Context.Provider value={{ market, locale, session, sessionError, refreshSession, act, busy, notice,
    path: (route = "/operations") => `/m/${market}/${locale}${route}`,
    login: async () => { await auth().signInWithOAuth({ provider: "password", popup: true }); await refreshSession(); },
    logout: async () => { await auth().signOut(); await refreshSession(); },
  }}>
    {children}
    {message && <Alert role="status" className="fixed bottom-5 right-5 z-50 w-auto max-w-[calc(100vw-2.5rem)]"><AlertDescription>{t(message)}</AlertDescription></Alert>}
  </Context.Provider>;
}
