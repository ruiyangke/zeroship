import { useState } from "react";
import { ChevronDown, ChevronUp } from "lucide-react";
import { Icon } from "@zeroship/ui";
import { SectionHeader } from "../components/SectionHeader";
import { companions } from "../content/applePage";
import type { CompanionItem } from "../types";

function panelId(name: string) {
  return `companion-panel-${name.replace(/\s+/g, "-").toLowerCase()}`;
}

function CompanionPicture({ item }: { item: CompanionItem }) {
  return (
    <picture>
      <source media="(max-width: 734px)" srcSet={item.imageSmall} />
      <source media="(max-width: 1068px)" srcSet={item.imageMedium} />
      <img src={item.image} alt="" />
    </picture>
  );
}

export function CompanionSection() {
  const [activeName, setActiveName] = useState(companions[0].name);
  const activeCompanion = companions.find((item) => item.name === activeName) ?? companions[0];

  return (
    <section className="companion-section" aria-labelledby="companion-title">
      <SectionHeader title="Significant others." id="companion-title" solo />
      <div className="companion-shell motion-reveal">
        <div className="companion-copy">
          {companions.map((item) => {
            const active = activeName === item.name;
            const DisclosureIcon = active ? ChevronUp : ChevronDown;
            return (
              <section className="companion-item" key={item.name}>
                <button
                  aria-expanded={active}
                  aria-controls={panelId(item.name)}
                  onClick={() => setActiveName(item.name)}
                  type="button"
                >
                  <span>{item.title}</span>
                  <Icon as={DisclosureIcon} size="sm" />
                </button>
                <div
                  aria-hidden={!active}
                  className="companion-item__body"
                  data-active={active ? "true" : undefined}
                  id={panelId(item.name)}
                >
                  <p>{item.body}</p>
                  <div className="companion-item__mobile-visual" aria-hidden="true">
                    <CompanionPicture item={item} />
                  </div>
                </div>
              </section>
            );
          })}
        </div>
        <div className="companion-visual" aria-live="polite">
          <CompanionPicture item={activeCompanion} />
        </div>
      </div>
    </section>
  );
}
