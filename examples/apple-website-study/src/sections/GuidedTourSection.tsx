import { Button } from "@zeroship/ui";
import { appleAssets } from "../assets/appleAssets";
import { SectionHeader } from "../components/SectionHeader";

export function GuidedTourSection() {
  return (
    <section className="guided-section" aria-labelledby="guided-section-title">
      <SectionHeader title="Take a closer look." id="guided-section-title" />
      <div className="guided-panel motion-reveal" aria-labelledby="guided-title">
        <picture className="guided-panel__media">
          <source media="(max-width: 734px)" srcSet={appleAssets.guidedTourSmall} />
          <source media="(max-width: 1068px)" srcSet={appleAssets.guidedTourMedium} />
          <img src={appleAssets.guidedTour} alt="" />
        </picture>
        <div className="guided-panel__copy">
          <h2 id="guided-title">
            A Guided Tour of
            <br />
            iPhone 17 Pro, iPhone Air,
            <br />
            and iPhone 17
          </h2>
          <Button asChild variant="gray">
            <a href="#guided-title">Watch the film</a>
          </Button>
        </div>
      </div>
    </section>
  );
}
