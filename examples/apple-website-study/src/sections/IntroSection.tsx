import { chapterItems } from "../content/applePage";

export function IntroSection() {
  return (
    <section className="page-intro" aria-labelledby="page-title">
      <div className="page-intro__inner">
        <h1 id="page-title">iPhone</h1>
        <nav className="chapter-nav" aria-label="iPhone lineup">
          {chapterItems.map((item) => (
            <a className="chapter-item" href="#lineup" key={item.label}>
              <img src={item.image} alt="" />
              <span className="chapter-item__label">{item.label}</span>
              {item.status ? <span className="chapter-item__status">{item.status}</span> : null}
            </a>
          ))}
        </nav>
      </div>
    </section>
  );
}
