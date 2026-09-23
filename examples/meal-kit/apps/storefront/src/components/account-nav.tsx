import { NavLink } from "react-router-dom";
import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import { useGather } from "../state";

export function AccountNav() {
  const { path } = useGather();
  const { _: t } = useLingui();
  return (
    <nav
      aria-label={t(msg`Account navigation`)}
      className="account-navigation flex flex-wrap gap-2 mb-8 border-b border-border pb-5"
    >
      {[
        ["/account", t(msg`My deliveries`)],
        ["/account/addresses", t(msg`Address book`)],
        ["/account/preferences", t(msg`Food preferences`)],
        ["/account/privacy", t(msg`Privacy & data`)],
      ].map(([route, label]) => (
        <NavLink
          key={route}
          end
          to={path(route)}
          className={({ isActive }) =>
            `rounded-full px-4 py-2 text-sm ${isActive ? "bg-primary text-primary-foreground" : "hover:bg-muted"}`
          }
        >
          {label}
        </NavLink>
      ))}
    </nav>
  );
}
