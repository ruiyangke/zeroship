import { useEffect, useRef, useState } from "react";
import { Link, useLocation } from "react-router-dom";
import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import {
  ChevronDown,
  LayoutDashboard,
  LogOut,
  MapPin,
  Package,
  ShieldCheck,
  SlidersHorizontal,
  UserRound,
} from "lucide-react";
import { useGather } from "../state";
import { Button } from "@gather/meal-kit/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@gather/meal-kit/components/ui/dropdown-menu";

export function AccountMenu() {
  const { _: t } = useLingui();
  const { session, path, act, logout, busy } = useGather();
  const location = useLocation();
  const [open, setOpen] = useState(false);
  const restoreFocus = useRef(true);
  useEffect(() => {
    setOpen(false);
  }, [location.pathname, location.search, location.hash]);
  if (!session?.user) return null;

  const links = [
    { route: "/account", label: t(msg`My deliveries`), Icon: Package },
    { route: "/account/addresses", label: t(msg`Address book`), Icon: MapPin },
    {
      route: "/account/preferences",
      label: t(msg`Food preferences`),
      Icon: SlidersHorizontal,
    },
    {
      route: "/account/privacy",
      label: t(msg`Privacy & data`),
      Icon: ShieldCheck,
    },
  ];
  return (
    <DropdownMenu
      open={open}
      onOpenChange={(next) => {
        if (next) restoreFocus.current = true;
        setOpen(next);
      }}
    >
      <DropdownMenuTrigger
        render={
          <Button
            variant="ghost"
            className="h-11 min-w-11 gap-2 px-2.5"
            aria-label={t(msg`My account`)}
          />
        }
      >
        <UserRound aria-hidden="true" />
        <span className="hidden md:inline">{t(msg`My account`)}</span>
        <ChevronDown aria-hidden="true" className="hidden md:block size-3" />
      </DropdownMenuTrigger>
      <DropdownMenuContent
        align="end"
        sideOffset={8}
        className="w-64 max-w-[calc(100vw-2rem)] p-1.5"
        aria-label={t(msg`My account`)}
        finalFocus={() => restoreFocus.current}
      >
        <DropdownMenuGroup>
          <DropdownMenuLabel className="space-y-1 px-3 py-2.5">
            <span className="block truncate text-sm font-medium text-foreground">
              {session.user.name}
            </span>
            <span className="block truncate font-normal">
              {session.user.email}
            </span>
          </DropdownMenuLabel>
          <DropdownMenuSeparator />
          {links.map(({ route, label, Icon }) => (
            <DropdownMenuItem
              key={route}
              render={<Link to={path(route)} />}
              className="min-h-11 gap-3 px-3"
              onClick={() => {
                restoreFocus.current = false;
              }}
            >
              <Icon aria-hidden="true" />
              {label}
            </DropdownMenuItem>
          ))}
        </DropdownMenuGroup>
        <DropdownMenuSeparator />
        <DropdownMenuItem
          className="min-h-11 gap-3 px-3"
          disabled={busy}
          onClick={() => void act(logout)}
        >
          <LogOut aria-hidden="true" />
          {t(msg`Sign out`)}
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
