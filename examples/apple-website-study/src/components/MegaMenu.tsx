import type { GlobalNavLabel } from "../content/applePage";
import type { MegaMenuModel } from "../types";

export function menuId(label: GlobalNavLabel) {
  return `apple-demo-mega-${label.toLowerCase().replace(/[^a-z0-9]+/g, "-")}`;
}

export function MegaMenu({ label, menu }: { label: GlobalNavLabel; menu: MegaMenuModel }) {
  return (
    <div className="apple-demo-mega" id={menuId(label)} role="region" aria-label={`${label} menu`}>
      <div className="apple-demo-mega__inner">
        <p className="apple-demo-mega__eyebrow">{menu.eyebrow}</p>
        <div className="apple-demo-mega__columns">
          {menu.columns.map((column) => (
            <section
              className={`apple-demo-mega__column${column.elevated ? " apple-demo-mega__column--elevated" : ""}`}
              key={column.heading}
            >
              <h2>{column.heading}</h2>
              <ul>
                {column.links.map((link) => (
                  <li key={link}>
                    <a href="#lineup">{link}</a>
                  </li>
                ))}
              </ul>
              {column.secondaryLinks ? (
                <ul className="apple-demo-mega__secondary-list">
                  {column.secondaryLinks.map((link) => (
                    <li key={link}>
                      <a href="#lineup">{link}</a>
                    </li>
                  ))}
                </ul>
              ) : null}
            </section>
          ))}
        </div>
      </div>
    </div>
  );
}
