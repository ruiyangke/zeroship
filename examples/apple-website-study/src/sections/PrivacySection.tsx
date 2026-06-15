import { Button } from "@zeroship/ui";
import { appleAssets } from "../assets/appleAssets";
import { SectionHeader } from "../components/SectionHeader";

export function PrivacySection() {
  return (
    <section className="privacy-section" id="privacy" aria-labelledby="privacy-section-title">
      <SectionHeader title="Privacy. That’s iPhone." id="privacy-section-title" />
      <div className="privacy-panel motion-reveal">
        <picture className="privacy-panel__media">
          <source media="(max-width: 734px)" srcSet={appleAssets.privacyBannerSmall} />
          <img src={appleAssets.privacyBanner} alt="" />
        </picture>
        <div className="privacy-panel__copy" aria-labelledby="privacy-title">
          <h2 id="privacy-title">Safari. A browser that’s actually private.</h2>
          <Button asChild>
            <a href="#privacy-title">
              Learn more
            </a>
          </Button>
        </div>
      </div>
    </section>
  );
}
