import { useRef } from "react";
import { Plus } from "lucide-react";
import { Button, Card, Icon } from "@zeroship/ui";
import { CarouselPaddles } from "../components/CarouselPaddles";
import { SectionHeader } from "../components/SectionHeader";
import { considerCards } from "../content/applePage";

export function ConsiderSection() {
  const railRef = useRef<HTMLDivElement | null>(null);

  return (
    <section className="consider-section" aria-labelledby="consider-title">
      <SectionHeader title="Get to know iPhone." id="consider-title" />
      <div className="carousel-shell">
        <div className="consider-rail" ref={railRef} role="list" aria-label="Get to know iPhone">
          {considerCards.map((item) => (
            <Card
              className={`consider-card consider-card--${item.tone} motion-reveal`}
              key={item.title}
              variant="surface"
              size="lg"
              role="listitem"
            >
              <picture className="consider-card__media">
                {item.imageSmall ? <source media="(max-width: 734px)" srcSet={item.imageSmall} /> : null}
                <img src={item.image} alt="" loading="lazy" />
              </picture>
              <Card.Header>
                <h3>{item.eyebrow}</h3>
                <p className="consider-card__headline">{item.title}</p>
              </Card.Header>
              <Button
                className="consider-card__more"
                variant="gray"
                size="small"
                aria-label={`Read more: ${item.eyebrow}`}
              >
                <Icon as={Plus} size="sm" />
              </Button>
            </Card>
          ))}
        </div>
        <CarouselPaddles label="Get to know iPhone controls" target={railRef} />
      </div>
    </section>
  );
}
