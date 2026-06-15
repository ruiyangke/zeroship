import { footerColumns } from "../content/applePage";

export function SiteIndex() {
  return (
    <section className="site-index" aria-label="Apple directory">
      <div className="site-index__inner">
        <h2>iPhone</h2>
        <div className="site-index__columns">
          {footerColumns.map((column, columnIndex) => (
            <section key={column.heading}>
              <h3>{column.heading}</h3>
              <ul>
                {column.links.map((link, linkIndex) => (
                  <li key={link}>
                    <a className={columnIndex === 0 && linkIndex < 6 ? "site-index__link--featured" : undefined} href="#lineup">
                      {link}
                    </a>
                  </li>
                ))}
              </ul>
            </section>
          ))}
        </div>
      </div>
    </section>
  );
}
