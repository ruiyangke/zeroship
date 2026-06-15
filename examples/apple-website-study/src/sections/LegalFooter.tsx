const legalFootnotes = [
  "* Trade-in values vary by the condition, year, and configuration of the eligible device you send in. Not every device qualifies for credit, and actual value is determined after inspection. The estimate can be applied to an eligible purchase, issued as a gift card, or adjusted if the device condition differs from the description. Taxes, activation charges, wireless service, carrier payoff amounts, installment balances, packaging, shipping, and account status are handled separately. Availability, timing, inspection standards, instant-credit eligibility, recycling options, and return windows vary by region, carrier, product, purchase channel, and payment method selected at checkout.",
  "** RCS messaging availability depends on carrier support, market availability, device software, account settings, and network conditions.",
];

const carrierDealTopics = [
  "AT&T iPhone 17 Pro and iPhone 17 Pro Max special deal",
  "AT&T iPhone Air special deal",
  "Boost Mobile iPhone special deal",
  "T-Mobile iPhone 17 and iPhone Air special deal",
  "Existing T-Mobile customer offer",
  "Add-a-line T-Mobile customer offer",
  "New T-Mobile customer offer",
  "Verizon iPhone 17 and iPhone Air special deal",
  "Existing Verizon customer offer",
  "New or add-a-line Verizon customer offer",
  "AT&T iPhone 16 special deal",
  "T-Mobile iPhone 16 special deal",
  "Existing T-Mobile iPhone 16 customer offer",
  "Add-a-line T-Mobile iPhone 16 customer offer",
  "New T-Mobile iPhone 16 customer offer",
  "Verizon iPhone 16 no-trade-in offer",
  "AT&T iPhone 17e special deal",
  "T-Mobile iPhone 17e special deal",
  "Existing T-Mobile iPhone 17e customer offer",
  "Add-a-line T-Mobile iPhone 17e customer offer",
  "New T-Mobile iPhone 17e customer offer",
  "Verizon iPhone 17e special deal",
  "Existing Verizon iPhone 17e customer offer",
  "New or add-a-line Verizon iPhone 17e customer offer",
  "Maximum bill credit conditions",
  "Trade-in and activation conditions",
];

const carrierDealParagraphs = carrierDealTopics.map((topic, index) => {
  const months = index % 3 === 0 ? "36" : "24";
  const plan = index % 2 === 0 ? "eligible unlimited" : "qualifying postpaid";
  return `${topic}: Monthly pricing, if shown, reflects promotional bill credits, instant credits, trade-in value, or carrier financing applied over ${months} months on a ${plan} plan. Device payment agreements, credit approval, tax on the full retail price, activation or upgrade charges, port-in requirements, autopay discounts, and service eligibility are set by the carrier. Credits begin after carrier validation and can stop if the line is canceled, transferred, upgraded early, paid off early, moved to an ineligible plan, or if the returned device does not match the represented model and condition. Promotional value is not cash, may not exceed the cost of the financed device, may require purchase and activation in the same transaction, and may be adjusted or revoked if carrier terms are not maintained. Trade-in devices must be owned by you, shipped on time, free of locks, undamaged beyond normal wear, and accepted after inspection. Wireless service is subject to coverage, roaming, data speed, network management, billing status, account standing, device compatibility, address validation, identity verification, and carrier availability rules. Taxes, fees, upgrade charges, shipping, optional insurance, early payoff amounts, plan changes, international roaming, and accessories are separate unless expressly stated. Offers, installment terms, device availability, purchase limits, eligible models, trade-in requirements, activation deadlines, promotional credit timing, and final monthly payment can change without notice and may not combine with every discount, consumer plan, business plan, education offer, government offer, employee offer, or reseller promotion.`;
});

type OrderedLegalNote =
  | { type: "text"; text: string }
  | { type: "paragraphs"; paragraphs: string[] };

const connectivityPricingNote = [
  "Pricing may include a connectivity discount that requires activation with a carrier at the time of purchase. If you select an unconnected purchase, choose a different carrier option, fail carrier activation, or change the line before validation is complete, the purchase price can be higher.",
  "Financing, taxes, shipping, wireless service, activation fees, upgrade fees, and any plan charges are separate and subject to credit approval, account status, carrier eligibility, and the terms shown during checkout.",
  "Prices, monthly payment examples, trade-in values, and promotional credits can change before purchase is completed.",
  "Carrier activation may require identity verification, eligible service, a compatible SIM or eSIM, and acceptance of separate wireless terms. Returned, exchanged, canceled, or delayed orders can affect the amount and timing of any instant discount, bill credit, refund, or monthly payment adjustment.",
].join(" ");

const installmentPricingNote = [
  "Monthly installment options require a qualifying card or carrier financing account, available credit, and acceptance of the applicable terms. Installment totals exclude taxes, shipping, wireless service, accessories, activation charges, and any carrier plan cost unless those amounts are specifically included during checkout.",
  "Rewards, discounts, promotional credits, purchase limits, billing timing, and return handling can vary by product, account status, payment method, billing address, shipping address, and checkout path.",
  "Installments begin when the order is processed and may continue after a return, exchange, or carrier plan change until the financing provider completes any adjustment. Card features, account services, cash back, and wallet integrations require compatible software, supported regions, and an account in good standing.",
  "Terms can change and additional restrictions may apply for pickup, delivery, preorder, backorder, replacement devices, refurbished devices, education purchases, business purchases, and products bought with other offers.",
  "If a financed product is returned, exchanged, or repriced, credits and refunds may be applied by the lender, carrier, or card issuer on a later statement. Missed payments, declined payment methods, account closure, loss of eligibility, or disputed charges can affect promotional pricing, rewards, and continued access to installment terms.",
  "The amount shown at checkout may differ from the amount billed by a carrier or financial partner because taxes, fees, surcharges, optional services, trade-in inspection, credit checks, shipping method, and order timing are calculated separately. Review all payment disclosures before placing the order.",
].join(" ");

const batteryTestingNote = [
  "Battery and performance testing use preproduction hardware and software with controlled network, media, brightness, and charging settings. Actual battery life and charging performance vary by configuration, battery age, temperature, signal strength, cellular technology, feature use, installed apps, notification activity, storage, and many other factors.",
  "Recharge cycles are limited and battery service may eventually be required. Comparisons are made against selected prior-generation devices using standard Apple test configurations.",
].join(" ");

const paymentAvailabilityNote = [
  "To use Apple Pay you need a supported card from a participating card issuer. To check whether your card is compatible, contact your card issuer. Apple Cash, Apple Card, Tap to Pay on iPhone, account services, installment products, savings products, and related payment features are provided by separate financial partners and are available only in selected countries, regions, businesses, and account types.",
  "Feature availability, setup, identity verification, security checks, transaction limits, merchant acceptance, card issuer support, device software, language, and account eligibility may vary. Some features require a compatible device, internet access, two-factor authentication, location settings, or additional terms.",
].join(" ");

const orderedLegalNotes: OrderedLegalNote[] = [
  { type: "text", text: "Compared with previous-generation iPhone." },
  {
    type: "text",
    text: "iPhone models are splash, water, and dust resistant under controlled laboratory conditions. Resistance is not permanent, can decrease with normal wear, and liquid damage is not covered under warranty.",
  },
  {
    type: "text",
    text: "Apple Intelligence features are in beta and may vary by language, region, device, and software version. Feature availability, supported apps, and system requirements can change.",
  },
  {
    type: "text",
    text: "Live Translation, visual intelligence, image tools, and writing tools depend on supported languages, region, model, and enabled settings. Some requests may require internet access.",
  },
  {
    type: "text",
    text: "Visual intelligence is available on compatible iPhone models. Capabilities vary by app, camera mode, location, and the information available in the scene.",
  },
  { type: "text", text: "Clean Up is available in beta. Compatible devices and software are required." },
  {
    type: "text",
    text: "Action mode and advanced camera features are available on select iPhone models. Performance can vary with lighting, motion, lens selection, and capture settings.",
  },
  { type: "text", text: "Environmental claims use mass-balance allocation and product-specific methodology." },
  {
    type: "text",
    text: "Satellite features are included for a limited period with activation of eligible iPhone models. Availability depends on country, carrier, satellite coverage, weather, terrain, and line of sight.",
  },
  {
    type: "text",
    text: connectivityPricingNote,
  },
  {
    type: "text",
    text: installmentPricingNote,
  },
  { type: "paragraphs", paragraphs: carrierDealParagraphs },
  {
    type: "text",
    text: batteryTestingNote,
  },
  {
    type: "text",
    text: "Data plan required. 5G and LTE are available in select markets and through select carriers. Speeds vary based on site conditions, carrier, and network congestion.",
  },
  { type: "text", text: "Accessories sold separately." },
  {
    type: "text",
    text: "AirTag precision finding range and speaker volume are compared with previous generation models and require compatible devices. Accuracy varies by environment and nearby interference.",
  },
  {
    type: "text",
    text: "Some nearby-finding features require iPhone and Apple Watch models with supported Ultra Wideband hardware. Availability varies by region.",
  },
];

const footerDirectoryColumns = [
  [
    {
      heading: "Shop and Learn",
      links: [
        "Store",
        "Mac",
        "iPad",
        "iPhone",
        "Watch",
        "Vision",
        "AirPods",
        "TV & Home",
        "AirTag",
        "Accessories",
        "Gift Cards",
      ],
    },
    {
      heading: "Apple Wallet",
      links: ["Wallet", "Apple Card", "Apple Pay", "Apple Cash"],
    },
  ],
  [
    {
      heading: "Account",
      links: ["Manage Your Apple Account", "Apple Store Account", "iCloud.com"],
    },
    {
      heading: "Entertainment",
      links: [
        "Apple One",
        "Apple TV",
        "Apple Music",
        "Apple Arcade",
        "Apple Fitness+",
        "Apple News+",
        "Apple Podcasts",
        "Apple Books",
        "App Store",
      ],
    },
  ],
  [
    {
      heading: "Apple Store",
      links: [
        "Find a Store",
        "Genius Bar",
        "Today at Apple",
        "Group Reservations",
        "Apple Camp",
        "Apple Store App",
        "Certified Refurbished",
        "Apple Trade In",
        "Financing",
        "Carrier Deals at Apple",
        "Order Status",
        "Shopping Help",
      ],
    },
  ],
  [
    {
      heading: "For Business",
      links: ["Apple and Business", "Shop for Business"],
    },
    {
      heading: "For Education",
      links: ["Apple and Education", "Shop for K-12", "Shop for College"],
    },
    {
      heading: "For Healthcare",
      links: ["Apple and Healthcare"],
    },
    {
      heading: "For Government",
      links: [
        "Apple and Government",
        "Shop for Veterans and Military",
        "Shop for State and Local Employees",
        "Shop for Federal Employees",
      ],
    },
  ],
  [
    {
      heading: "Apple Values",
      links: [
        "Accessibility",
        "Education",
        "Environment",
        "Inclusion and Diversity",
        "Privacy",
        "Racial Equity and Justice",
        "Supply Chain Innovation",
      ],
    },
    {
      heading: "About Apple",
      links: ["Newsroom", "Apple Leadership", "Career Opportunities", "Investors", "Ethics & Compliance", "Events", "Contact Apple"],
    },
  ],
];

export function LegalFooter() {
  return (
    <footer className="legal-footer" aria-label="Apple footer">
      <div className="legal-footer__inner">
        <section className="legal-footer__notes" aria-label="Legal notes">
          <h2>Apple Footer</h2>
          <ul className="legal-footer__plain-notes legal-footer__trade-notes">
            {legalFootnotes.map((note, index) => (
              <li key={`${index}-${note.slice(0, 24)}`}>{note}</li>
            ))}
          </ul>
          <ol className="legal-footer__ordered-notes">
            {orderedLegalNotes.map((note, index) => (
              <li key={`${index}-${note.type}`}>
                {note.type === "text"
                  ? note.text
                  : note.paragraphs.map((paragraph) => <p key={paragraph.slice(0, 48)}>{paragraph}</p>)}
              </li>
            ))}
          </ol>
          <ul className="legal-footer__plain-notes legal-footer__payment-notes">
            <li>{paymentAvailabilityNote}</li>
          </ul>
        </section>

        <nav className="legal-footer__breadcrumbs" aria-label="Breadcrumb">
          <a href="#lineup" aria-label="Apple">
            
          </a>
          <span aria-hidden="true">›</span>
          <a href="#lineup">iPhone</a>
        </nav>

        <nav className="legal-footer__directory" aria-label="Apple footer directory">
          {footerDirectoryColumns.map((column, columnIndex) => (
            <div className="legal-footer__directory-column" key={columnIndex}>
              {column.map((section) => (
                <section key={section.heading}>
                  <h3>{section.heading}</h3>
                  <ul>
                    {section.links.map((link) => (
                      <li key={link}>
                        <a href="#lineup">{link}</a>
                      </li>
                    ))}
                  </ul>
                </section>
              ))}
            </div>
          ))}
        </nav>

        <section className="legal-footer__bottom" aria-label="Footer legal">
          <p className="legal-footer__shop">
            More ways to shop: <a href="#lineup">Find an Apple Store</a> or <a href="#lineup">other retailer</a> near
            you. Or call <a href="#lineup">1-800-MY-APPLE</a> (1-800-692-7753).
          </p>
          <p className="legal-footer__locale">United States</p>
          <p className="legal-footer__copyright">Copyright © 2026 Apple Inc. All rights reserved.</p>
          <nav className="legal-footer__legal-links" aria-label="Legal">
            <a href="#lineup">Privacy Policy</a>
            <a href="#lineup">Terms of Use</a>
            <a href="#lineup">Sales and Refunds</a>
            <a href="#lineup">Legal</a>
            <a href="#lineup">Site Map</a>
          </nav>
        </section>
      </div>
    </footer>
  );
}
