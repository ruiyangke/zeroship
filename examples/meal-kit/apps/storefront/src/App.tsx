import { Select, SelectItem } from "@gather/meal-kit/components/select-field";
import {
  Sheet,
  SheetContent,
  SheetTitle,
  SheetDescription,
  SheetTrigger,
} from "@gather/meal-kit/components/ui/sheet";
import { useLingui } from "@lingui/react";
import { msg, plural } from "@lingui/core/macro";
import { useEffect, useState } from "react";
import {
  Link,
  NavLink,
  Navigate,
  Route,
  Routes,
  useLocation,
  useNavigate,
  useParams,
} from "react-router-dom";
import { Leaf, Menu, ShoppingBag } from "lucide-react";
import { GatherProvider, useGather } from "./state";
import { DraftFeedback } from "./components/draft-feedback";
import { AccountMenu } from "./components/account-menu";
import { PageTransition } from "@gather/meal-kit/components/page-transition";
import { KitchenTimers } from "./components/cooking-timer";
import { markets, type MarketId } from "@gather/meal-kit/catalog";
import { Button, CtaLink, SectionTitle } from "@gather/meal-kit/components/shared";
import { Plans } from "./plan-wizard";
import { BoxPage } from "./box";
import { rememberMarket } from "./entry";
import { Home, MenuPage, RecipePage, CookPage, Help } from "./pages";
import { Checkout, Account, OrderPage, SignIn } from "./customer";
import { AddressesPage, PreferencesPage, PrivacyPage } from "./settings";
import { LocaleProvider } from "@gather/meal-kit/i18n";
import { locales, resolveLocale, languageName, type Locale } from "@gather/meal-kit/locales";

export default function App() {
  const params = useParams();
  const location = useLocation();
  const market =
    params.market && Object.hasOwn(markets, params.market)
      ? params.market
      : "us";
  const locale = resolveLocale(params.locale);
  if (market !== params.market || locale !== params.locale) {
    return (
      <Navigate
        replace
        to={`/m/${market}/${locale}${params["*"] ? `/${params["*"]}` : ""}${location.search}${location.hash}`}
      />
    );
  }
  return (
    <LocaleProvider locale={locale}>
      <GatherProvider key={market}>
        <Shell />
      </GatherProvider>
    </LocaleProvider>
  );
}
function Shell() {
  const { locale, market, path, session, login, act, cart } = useGather();
  const { _: t } = useLingui();
  const [menu, setMenu] = useState(false);
  const navigate = useNavigate();
  const location = useLocation();
  useEffect(() => {
    rememberMarket(market);
  }, [market]);
  useEffect(() => {
    document.title = t(msg`Gather — good food, closer to home`);
    document
      .querySelector('meta[name="description"]')
      ?.setAttribute(
        "content",
        t(
          msg`Make room for a good dinner. Discover delicious recipes, build your box, and enjoy ingredients ready for your kitchen.`,
        ),
      );
  }, [t]);
  useEffect(() => {
    setMenu(false);
  }, [location.pathname, location.hash]);
  const links = [
    ["/menu", t(msg`This week's menu`)],
    ["/plans", t(msg`Our plans`)],
    ["/help", t(msg`How it works`)],
  ];
  return (
    <>
      <a className="skip-link" href="#main">
        {t(msg`Skip to content`)}
      </a>
      <header>
        <div className="announcement">
          {t(msg`Store preview · No charges or deliveries`)}
        </div>
        <div className="page">
          <div className="navbar">
            <Link
              to={path()}
              className="brand"
              aria-label={t(msg`Gather home`)}
            >
              gather
              <Leaf size={22} strokeWidth={1.5} />
            </Link>
            <nav aria-label={t(msg`Main navigation`)} className="desktop-nav">
              {links.map(([route, label]) => (
                <NavLink
                  className={({ isActive }) =>
                    `nav-link ${isActive ? "active" : ""}`
                  }
                  key={route}
                  to={path(route)}
                >
                  {label}
                </NavLink>
              ))}
            </nav>
            <div className="nav-right">
              <Select
                className="market-picker"
                aria-label={t(msg`Delivery country`)}
                value={market}
                onValueChange={(selectedValue) =>
                  navigate(
                    location.pathname.includes("/operations")
                      ? `/m/${selectedValue}/${locale}/operations`
                      : location.pathname.includes("/account")
                        ? `/m/${selectedValue}/${locale}/account`
                        : `/m/${selectedValue}/${locale}/plans`,
                  )
                }
              >
                {Object.entries(markets).map(([id, m]) => (
                  <SelectItem value={id} key={id}>
                    {t(m.name)}
                  </SelectItem>
                ))}
              </Select>
              <Select
                className="language-picker"
                aria-label={t(msg`Language`)}
                value={locale}
                onValueChange={(selectedValue) =>
                  navigate(
                    window.location.pathname.replace(
                      /^(\/m\/[^/]+\/)\w[\w-]*/,
                      `$1${selectedValue}`,
                    ) +
                      window.location.search +
                      window.location.hash,
                  )
                }
              >
                {(Object.keys(locales) as Locale[]).map((language) => (
                  <SelectItem
                    key={language}
                    value={language}
                    lang={locales[language].tag}
                  >
                    {languageName(language)}
                  </SelectItem>
                ))}
              </Select>
              {session?.user ? (
                <AccountMenu />
              ) : (
                <Button
                  className="desktop-only"
                  variant="outline"
                  onClick={() => act(login)}
                >
                  {t(msg`Log in`)}
                </Button>
              )}
              <Link
                to={path("/box")}
                className="relative"
                aria-label={t(
                  msg({
                    message: plural(cart.recipeIds.length, {
                      one: "Your box, # meal",
                      other: "Your box, # meals",
                    }),
                  }),
                )}
              >
                <ShoppingBag size={21} />
                {cart.recipeIds.length > 0 && (
                  <span className="cart-count">{cart.recipeIds.length}</span>
                )}
              </Link>
              <Sheet open={menu} onOpenChange={setMenu}>
                <SheetTrigger
                  render={
                    <Button
                      size="icon"
                      variant="ghost"
                      className="mobile-menu-button"
                      aria-label={t(msg`Open navigation`)}
                    />
                  }
                >
                  <Menu />
                </SheetTrigger>
                <SheetContent side="bottom" className="navigation-sheet">
                  <SheetTitle>{t(msg`Explore Gather`)}</SheetTitle>
                  <SheetDescription>
                    {t(msg`Choose meals or manage your deliveries.`)}
                  </SheetDescription>
                  <nav
                    aria-label={t(msg`Main navigation`)}
                    className="flex flex-col gap-5 py-3"
                  >
                    {links.map(([route, label]) => (
                      <Link
                        key={route}
                        to={path(route)}
                        onClick={() => setMenu(false)}
                      >
                        {label}
                      </Link>
                    ))}
                    <Link to={path("/box")} onClick={() => setMenu(false)}>
                      {t(msg`Your box`)}
                    </Link>
                    <Link to={path("/account")} onClick={() => setMenu(false)}>
                      {t(msg`My account`)}
                    </Link>
                    {!session?.user && (
                      <Button
                        onClick={() => {
                          setMenu(false);
                          void act(login);
                        }}
                      >
                        {t(msg`Log in`)}
                      </Button>
                    )}
                  </nav>
                </SheetContent>
              </Sheet>
            </div>
          </div>
        </div>
      </header>
      <main id="main" className="page" tabIndex={-1}>
        <KitchenTimers />
        <DraftFeedback />
        <PageTransition>
          <Routes>
            <Route index element={<Home />} />
            <Route path="menu" element={<MenuPage />} />
            <Route path="plans" element={<Plans />} />
            <Route path="recipes/:recipeId" element={<RecipePage />} />
            <Route path="box" element={<BoxPage />} />
            <Route path="checkout" element={<Checkout />} />
            <Route path="account" element={<Account />} />
            <Route path="account/addresses" element={<AddressesPage />} />
            <Route path="account/preferences" element={<PreferencesPage />} />
            <Route path="account/privacy" element={<PrivacyPage />} />
            <Route path="orders/:id" element={<OrderPage />} />
            <Route
              path="orders/:id/cook/:recipeId"
              element={
                <SignIn>
                  <CookPage />
                </SignIn>
              }
            />
            <Route path="help" element={<Help />} />
            <Route path="*" element={<NotFound />} />
          </Routes>
        </PageTransition>
      </main>
      <footer className="page">
        <div className="footer">
          <div>
            <Link to={path()} className="brand">
              gather
              <Leaf size={20} />
            </Link>
            <p className="mt-3 text-xs text-muted-foreground">
              {t(msg`Good food. A little more together.`)}
            </p>
          </div>
          <div className="footer-links">
            <div>
              <strong>{t(msg`At your table`)}</strong>
              <Link to={path("/menu")}>{t(msg`Explore the menu`)}</Link>
              <Link to={path("/plans")}>{t(msg`Find your plan`)}</Link>
            </div>
            <div>
              <strong>{t(msg`Here to help`)}</strong>
              <Link to={path("/help")}>{t(msg`Delivery & FAQs`)}</Link>
              <Link to={path("/account")}>{t(msg`Manage your plan`)}</Link>
              {session?.staff && (
                <Link to={path("/operations")}>{t(msg`Operations`)}</Link>
              )}
            </div>
          </div>
        </div>
        <div className="footer-bottom">
          <span>Gather</span>
          <Link to={path("/account/privacy")}>{t(msg`Privacy and data`)}</Link>
        </div>
      </footer>
    </>
  );
}

function NotFound() {
  const { _: t } = useLingui();
  const { path } = useGather();
  return (
    <section className="section">
      <SectionTitle
        title={t(msg`We couldn't find that page`)}
        body={t(
          msg`The link may have changed. Explore the menu or return to your deliveries.`,
        )}
      />
      <div className="flex gap-4 flex-wrap">
        <CtaLink to={path("/menu")}>{t(msg`Explore the menu`)}</CtaLink>
        <CtaLink outline to={path("/account")}>
          {t(msg`My deliveries`)}
        </CtaLink>
      </div>
    </section>
  );
}
