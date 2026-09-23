import { useEffect } from "react";
import { Link, Navigate, Route, Routes, useNavigate, useParams } from "react-router-dom";
import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import { LogOut, UserRound } from "lucide-react";
import { LocaleProvider } from "@gather/meal-kit/i18n";
import { locales, resolveLocale, languageName, type Locale } from "@gather/meal-kit/locales";
import { markets } from "@gather/meal-kit/catalog";
import { Select, SelectItem } from "@gather/meal-kit/components/select-field";
import { Button } from "@gather/meal-kit/components/ui/button";
import { DropdownMenu, DropdownMenuContent, DropdownMenuGroup, DropdownMenuItem, DropdownMenuLabel, DropdownMenuSeparator, DropdownMenuTrigger } from "@gather/meal-kit/components/ui/dropdown-menu";
import { PageTransition } from "@gather/meal-kit/components/page-transition";
import { GatherProvider, useGather } from "./state";
import { Operations } from "./operations";

export default function App() {
  const params = useParams();
  const market = params.market && Object.hasOwn(markets, params.market) ? params.market : "us";
  const locale = resolveLocale(params.locale);
  if (market !== params.market || locale !== params.locale) return <Navigate replace to={`/m/${market}/${locale}/operations`} />;
  return <LocaleProvider locale={locale}><GatherProvider key={market}><Shell /></GatherProvider></LocaleProvider>;
}
function Shell() {
  const { market, locale, session, act, login, logout, path, busy } = useGather();
  const { _: t } = useLingui();
  const navigate = useNavigate();
  useEffect(() => { document.title = t(msg`Gather back office`); }, [t]);
  return <>
    <a className="skip-link" href="#main">{t(msg`Skip to content`)}</a>
    <header className="border-b bg-background">
      <div className="page flex min-h-20 items-center justify-between gap-3 flex-wrap py-3">
        <Link to={path()} className="brand text-xl" aria-label={t(msg`Gather back office`)}>gather <span className="font-sans text-xs font-normal">{t(msg`Operations`)}</span></Link>
        <div className="flex flex-wrap items-center gap-2">
          <Select aria-label={t(msg`Delivery country`)} value={market} onValueChange={value => navigate(`/m/${value}/${locale}/operations`)} className="w-auto">
            {Object.entries(markets).map(([id, country]) => <SelectItem key={id} value={id}>{t(country.name)}</SelectItem>)}
          </Select>
          <Select aria-label={t(msg`Language`)} value={locale} onValueChange={value => navigate(`/m/${market}/${value}/operations`)} className="w-auto">
            {(Object.keys(locales) as Locale[]).map(id => <SelectItem key={id} value={id} lang={locales[id].tag}>{languageName(id)}</SelectItem>)}
          </Select>
          {session?.user ? <DropdownMenu>
            <DropdownMenuTrigger render={<Button variant="ghost" aria-label={t(msg`My account`)} />}><UserRound aria-hidden="true" /></DropdownMenuTrigger>
            <DropdownMenuContent align="end" className="w-64 max-w-[calc(100vw-2rem)]">
              <DropdownMenuGroup><DropdownMenuLabel className="break-all">{session.user.name}<span className="block text-xs font-normal">{session.user.email}</span></DropdownMenuLabel></DropdownMenuGroup>
              <DropdownMenuSeparator />
              <DropdownMenuItem disabled={busy} onClick={() => void act(logout)}><LogOut aria-hidden="true" />{t(msg`Sign out`)}</DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu> : <Button variant="ghost" disabled={busy} onClick={() => void act(login)}>{t(msg`Log in`)}</Button>}
        </div>
      </div>
    </header>
    <main id="main" className="page" tabIndex={-1}>
      <PageTransition><Routes><Route path="operations" element={<Operations />} /><Route path="*" element={<Navigate replace to={path()} />} /></Routes></PageTransition>
    </main>
  </>;
}
