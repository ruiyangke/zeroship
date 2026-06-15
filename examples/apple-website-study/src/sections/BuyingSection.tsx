import { useRef } from "react";
import { Plus } from "lucide-react";
import { Button, Card, Dialog, Icon } from "@zeroship/ui";
import { CarouselPaddles } from "../components/CarouselPaddles";
import { SectionHeader } from "../components/SectionHeader";
import { incentives } from "../content/applePage";

const buyingDetails: Record<
  string,
  {
    headline: string;
    body: string;
    points: string[];
    action: string;
  }
> = {
  "Apple Trade In": {
    headline: "Trade in your eligible smartphone for credit.",
    body: "Just answer a few questions to get your estimated trade-in value. If your device is eligible, you can apply the credit toward a new iPhone or get an Apple Gift Card you can use anytime.",
    points: [
      "Get an instant estimate online.",
      "Send in your device or bring it to an Apple Store.",
      "Apple can recycle it for free if it is not eligible for credit.",
    ],
    action: "Learn about Apple Trade In",
  },
  "Ways to Buy": {
    headline: "Choose the payment option that works for you.",
    body: "Apple gives you flexible ways to pay for your new iPhone, including Apple Card Monthly Installments when you check out with Apple Card.",
    points: [
      "Pay over time, interest-free with Apple Card Monthly Installments.",
      "Use your preferred payment method at checkout.",
      "Add AppleCare+ and accessories in the same order.",
    ],
    action: "Explore ways to buy",
  },
  "Carrier Deals at Apple": {
    headline: "Get carrier deals without the carrier-store runaround.",
    body: "Apple can help you compare offers, connect your new iPhone to your carrier, and trade in an eligible device for additional savings.",
    points: [
      "See offers from major carriers in one place.",
      "Activate your iPhone online or in store.",
      "Get help transferring your number and plan.",
    ],
    action: "See carrier deals",
  },
  "Personal Setup": {
    headline: "Meet your new iPhone with a Specialist.",
    body: "After you buy, schedule a free online session to get set up, move your data, and discover the features that matter most to you.",
    points: [
      "Set up Face ID, Apple Account, iCloud, and more.",
      "Learn camera, privacy, and Apple Intelligence features.",
      "Ask questions one on one, whenever you are ready.",
    ],
    action: "Book Personal Setup",
  },
  "Delivery and Pickup": {
    headline: "Get your iPhone the way you want.",
    body: "Choose free delivery, two-hour delivery from an Apple Store, or convenient pickup at a nearby store when inventory is available.",
    points: [
      "Pick a delivery window at checkout.",
      "Choose pickup with setup help in store.",
      "Track your order from purchase to arrival.",
    ],
    action: "View delivery and pickup",
  },
  "Guided Shopping": {
    headline: "Shop live with an Apple Specialist.",
    body: "Get help comparing models, choosing storage, finding the right carrier offer, and completing your purchase online or in store.",
    points: [
      "Ask product questions in real time.",
      "Compare models, colors, and payment options.",
      "Get one-on-one help before you buy.",
    ],
    action: "Shop with a Specialist",
  },
  "Apple Store App": {
    headline: "A more personal way to shop Apple.",
    body: "The Apple Store app helps you save favorite products, compare options, check local availability, and get recommendations tailored to the devices you own.",
    points: [
      "Shop iPhone, accessories, and services.",
      "Track orders and pickup details.",
      "Get a tailored shopping experience in one place.",
    ],
    action: "Explore the Apple Store app",
  },
};

export function BuyingSection() {
  const railRef = useRef<HTMLDivElement | null>(null);

  return (
    <section className="buying-section" id="buying" aria-labelledby="buying-title">
      <SectionHeader
        title="Why Apple is the best place to buy iPhone."
        id="buying-title"
        actionLabel="Shop iPhone"
        actionHref="#buying"
      />

      <div className="carousel-shell">
        <div className="incentive-rail" ref={railRef} role="list" aria-label="Buying support">
          {incentives.map((item) => {
            const match = item.body.match(/^(.+?\.(?:\d+)?)\s+(.*)$/);
            const headlineText = match?.[1] ?? item.body;
            const body = match?.[2] ?? "";
            const detail = buyingDetails[item.title];

            return (
              <Card
                className="incentive-card motion-reveal"
                key={item.title}
                variant="surface"
                size="lg"
                role="listitem"
              >
                <picture className="incentive-card__image">
                  {item.imageSmall ? <source media="(max-width: 734px)" srcSet={item.imageSmall} /> : null}
                  <img src={item.image} alt="" loading="eager" decoding="async" />
                </picture>
                <Card.Header>
                  <h3 className="incentive-card__label">{item.title}</h3>
                  <p className="incentive-card__headline">{headlineText}</p>
                  {body ? <Card.Description>{body}</Card.Description> : null}
                </Card.Header>
                {detail ? (
                  <Dialog>
                    <Dialog.Trigger
                      render={
                        <Button
                          className="incentive-card__more"
                          variant="gray"
                          size="small"
                          aria-label={`Read more: ${item.title}`}
                        >
                          <Icon as={Plus} size="sm" />
                        </Button>
                      }
                    />
                    <Dialog.Portal>
                      <Dialog.Backdrop tint="material" />
                      <Dialog.Popup className="buying-detail-dialog" size="lg" placement="top">
                        <Dialog.Header className="buying-detail-dialog__header">
                          <span className="buying-detail-dialog__eyebrow">{item.title}</span>
                          <Dialog.Title>{detail.headline}</Dialog.Title>
                        </Dialog.Header>
                        <Dialog.Body className="buying-detail-dialog__body">
                          <p>{detail.body}</p>
                          <ul>
                            {detail.points.map((point) => (
                              <li key={point}>{point}</li>
                            ))}
                          </ul>
                        </Dialog.Body>
                        <Dialog.Footer className="buying-detail-dialog__footer">
                          <Button variant="plain" size="small">
                            {detail.action}
                          </Button>
                          <Dialog.Close size="small">Done</Dialog.Close>
                        </Dialog.Footer>
                      </Dialog.Popup>
                    </Dialog.Portal>
                  </Dialog>
                ) : null}
              </Card>
            );
          })}
        </div>
        <CarouselPaddles label="Buying support controls" target={railRef} />
      </div>
    </section>
  );
}
