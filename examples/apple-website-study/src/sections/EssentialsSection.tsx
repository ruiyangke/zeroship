import { ChevronRight } from "lucide-react";
import { Button, Card } from "@zeroship/ui";
import { SectionHeader } from "../components/SectionHeader";
import { essentials } from "../content/applePage";

export function EssentialsSection() {
  return (
    <section className="essentials-section" aria-labelledby="essentials-title">
      <SectionHeader
        title="iPhone essentials."
        id="essentials-title"
        actionLabel="All iPhone accessories"
        actionHref="#essentials-title"
      />
      <div className="essentials-grid">
        {essentials.map((item) => (
          <Card className="essential-card motion-reveal" key={item.title} variant="surface" size="lg">
            <Card.Header>
              <h3>
                {item.eyebrow ? <span className="essential-card__eyebrow">{item.eyebrow}</span> : null}
                <span className="essential-card__headline">{item.title}</span>
              </h3>
              <p className="essential-card__body">{item.body}</p>
              <Button asChild variant="plain">
                <a href="#essentials-title">
                  {item.title === "AirTag" ? "Buy" : "Shop iPhone accessories"}
                  <ChevronRight aria-hidden="true" size={16} strokeWidth={2} />
                </a>
              </Button>
            </Card.Header>
            <div className="essential-card__asset">
              <picture>
                {item.imageSmall ? <source media="(max-width: 734px)" srcSet={item.imageSmall} /> : null}
                <img src={item.image} alt="" />
              </picture>
            </div>
          </Card>
        ))}
      </div>
    </section>
  );
}
