import { useEffect, useRef, useState, type FocusEvent } from "react";
import { Menu, Search, ShoppingBag } from "lucide-react";
import { globalNav, megaMenus, type GlobalNavLabel } from "../content/applePage";
import { MegaMenu, menuId } from "./MegaMenu";
import { NavIconButton } from "./NavIconButton";

export function GlobalNav() {
  const [activeMenu, setActiveMenu] = useState<GlobalNavLabel | null>(null);
  const closeTimer = useRef<number | null>(null);
  const navRef = useRef<HTMLElement | null>(null);

  const clearCloseTimer = () => {
    if (closeTimer.current !== null) {
      window.clearTimeout(closeTimer.current);
      closeTimer.current = null;
    }
  };

  const openMenu = (label: GlobalNavLabel) => {
    clearCloseTimer();
    setActiveMenu(label);
  };

  const scheduleCloseMenu = () => {
    clearCloseTimer();
    closeTimer.current = window.setTimeout(() => setActiveMenu(null), 140);
  };

  const closeMenu = () => {
    clearCloseTimer();
    setActiveMenu(null);
  };

  const onHeaderBlur = (event: FocusEvent<HTMLElement>) => {
    if (!event.currentTarget.contains(event.relatedTarget)) closeMenu();
  };

  useEffect(() => {
    return () => clearCloseTimer();
  }, []);

  useEffect(() => {
    if (activeMenu == null) return undefined;
    const onDocumentKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") closeMenu();
    };
    document.addEventListener("keydown", onDocumentKeyDown);
    return () => document.removeEventListener("keydown", onDocumentKeyDown);
  }, [activeMenu]);

  return (
    <>
      <header
        className="apple-demo-globalnav"
        data-menu-open={activeMenu != null ? "true" : undefined}
        onBlur={onHeaderBlur}
        onKeyDown={(event) => {
          if (event.key === "Escape") closeMenu();
        }}
        onMouseEnter={clearCloseTimer}
        onMouseLeave={scheduleCloseMenu}
        ref={navRef}
      >
        <div className="apple-demo-globalnav__inner">
          <a className="apple-demo-mark" href="#" aria-label="Apple home">
            
          </a>
          <nav className="apple-demo-globalnav__links" aria-label="Global">
            {globalNav.map((item) => (
              <button
                aria-controls={menuId(item)}
                aria-expanded={activeMenu === item}
                className="apple-demo-globalnav__link"
                key={item}
                onClick={() => (activeMenu === item ? closeMenu() : openMenu(item))}
                onFocus={() => openMenu(item)}
                onMouseEnter={() => openMenu(item)}
                type="button"
              >
                {item}
              </button>
            ))}
          </nav>
          <div className="apple-demo-globalnav__actions">
            <NavIconButton label="Search" icon={Search} />
            <NavIconButton label="Shopping bag" icon={ShoppingBag} />
            <span className="apple-demo-globalnav__mobile-menu">
              <NavIconButton label="Menu" icon={Menu} />
            </span>
          </div>
        </div>
        {activeMenu ? <MegaMenu label={activeMenu} menu={megaMenus[activeMenu]} /> : null}
      </header>
      <div
        className="apple-demo-menu-curtain"
        data-visible={activeMenu != null ? "true" : undefined}
        onClick={closeMenu}
        aria-hidden="true"
      />
    </>
  );
}
