import { ChevronRight } from "lucide-react";
import { ThemeProvider } from "@zeroship/ui";
import { GlobalNav } from "./components/GlobalNav";
import { useMotionReveal } from "./hooks/useMotionReveal";
import { BuyingSection } from "./sections/BuyingSection";
import { CompanionSection } from "./sections/CompanionSection";
import { ConsiderSection } from "./sections/ConsiderSection";
import { EssentialsSection } from "./sections/EssentialsSection";
import { GuidedTourSection } from "./sections/GuidedTourSection";
import { IntroSection } from "./sections/IntroSection";
import { LegalFooter } from "./sections/LegalFooter";
import { LineupSection } from "./sections/LineupSection";
import { PrivacySection } from "./sections/PrivacySection";
import { SiteIndex } from "./sections/SiteIndex";

function ShoppingRibbon() {
  return (
    <aside className="apple-demo-ribbon" aria-label="Shopping guidance">
      <p>
        Get up to $195–$695 in credit toward iPhone 17, iPhone Air, or iPhone 17 Pro when you trade in iPhone 13 or
        higher.*
        <a href="#buying">
          Shop iPhone
          <ChevronRight aria-hidden="true" size={14} strokeWidth={2} />
        </a>
      </p>
    </aside>
  );
}

function AppleWebsiteStudy() {
  useMotionReveal();

  return (
    <div className="apple-demo">
      <GlobalNav />

      <main>
        <ShoppingRibbon />
        <IntroSection />
        <LineupSection />
        <GuidedTourSection />
        <BuyingSection />
        <ConsiderSection />
        <PrivacySection />
        <EssentialsSection />
        <CompanionSection />
        <SiteIndex />
      </main>

      <LegalFooter />
    </div>
  );
}

export function App() {
  return (
    <ThemeProvider defaultTheme="crystal-light" persist={false}>
      <AppleWebsiteStudy />
    </ThemeProvider>
  );
}
