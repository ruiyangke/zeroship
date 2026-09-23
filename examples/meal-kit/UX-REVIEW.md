# Customer journey review

The [website plan](../../docs/proposals/2026-09-11-meal-kit-example.md) remains
the scope authority. This review records product decisions and their reasons;
the implementation ledger tracks unfinished capabilities.

## Where the journey broke down

| Customer situation | Problem | Resulting behavior |
| --- | --- | --- |
| Arriving without a country | The root assumed a US delivery | Choose a country; remember that choice independently of language |
| Starting a box | Eligibility, portions, frequency and dates competed in a long form | Check the delivery area before configuring the box; show the purchase steps and a clear back action |
| Moving through purchase steps | Content and summary columns changed width between screens | Use shared purchase width and column tokens with a consistent mobile layout |
| Leaving a shared device | Sign out was buried in the footer | Keep sign out and account shortcuts in a header dropdown on desktop and mobile |
| Opening the shopping bag | Different links opened the menu or delivery form | Every bag action opens the box review |
| Reducing box size | Selected meals silently disappeared | Keep selections visible and let the customer choose which to remove |
| Returning to an old selection | The delivery date silently advanced | Preserve the previous date and explicitly request a replacement |
| Returning from another device | Selections existed only in the current browser | Restore the account's country-specific box; ask before replacing different choices |
| Reviewing unavailable meals | Missing recipes could count toward a complete box | Mark affected selections, retain a removal action and prevent checkout until repaired |
| Changing an address | The entered destination could differ from the priced destination | Refresh the server total from the entered address and require consent for changed order terms |
| Confirming a future box | A total appeared without the meals or address being reviewed | Show the saved meals, portions, address, date and current total together |
| Viewing order history | An empty history told returning customers to start their first box | Use view-specific empty states and preserve the selected view in the URL |
| Following payment recovery | Payment events were under a delivery heading | Present order updates and keep payment review distinct from fulfillment |
| Following a purchased recipe | A larger serving count could imply extra delivered ingredients | Fix quantities to the packed portions; allow unit changes and keep serving scaling in public browsing |
| Leaving a recipe while a timer runs | A timer could disappear with its page | Show active timers across navigation and link back to the relevant step |
| Editing feedback from another device | A stale form could overwrite a newer response | Preserve the entered response on conflict and provide an explicit action to load the saved feedback |

## Control decisions

Use the shadcn CLI configuration in `components.json`. Generate primitives into
`packages/shared/src/components/ui`; keep application decisions in composed components outside
that directory. Tailwind and theme tokens own the brand. Generated controls still
need application labels, localization and behavioral verification.

| Decision | Control and interaction |
| --- | --- |
| Country and language | Select; show the current value and keep each choice independent |
| Delivery eligibility | Local address controls, postal autofill where relevant, dependent administrative selectors for China; request the full street address at checkout |
| People per meal | Slider with visible value and precise increment/decrement actions; the shared `servingRange` also bounds server validation |
| Meals per box | Radio Group with visible options; preserve selected meals when the target changes |
| Purchase frequency | Radio Group with the consequences beside each option; reflect the actual confirmation requirement for future boxes |
| Delivery day | Calendar with enabled days supplied by published menus and delivery capacity; local labels and keyboard navigation |
| Recipe category | Toggle Group with a visible selected state |
| Allergen exclusions | Checkbox choices in a Sheet; clearing search must preserve exclusions |
| Saved delivery address | Radio Group showing the label and street; offer an explicit new-address choice |
| Address entry | Field, Input, Select and Textarea; use contact autofill, suitable mobile keyboards and errors beside the affected field |
| Checkout consent | Checkbox bound to the reviewed destination and price; disable it while the total is pending |
| Account views | Tabs with a URL-backed selection so reload and back navigation preserve context |
| Page and wizard navigation | Shared Motion transitions keep the shell steady; wizard direction follows progress, tabs retain focus, and reduced-motion preferences disable animation |
| Preferences | Checkbox groups for independent choices; Radio Group for mutually exclusive units; explicit Save action |
| Cooking preparation | Checkbox ingredient list, toggleable steps and Radio Group unit choices; printed instructions retain quantities and omit interactive controls |
| Step timers | Collapsible duration entry with labeled numeric fields; pause, resume, reset and dismiss actions; completion status without announcing every tick |
| Recipe rating | Radio Group with labeled star choices, optional cook-again choice and Textarea comment; explicit save and update actions |
| Support | Radio Group for the issue category and Textarea for the explanation; keep the order context visible |
| Destructive changes | Dialog explaining the affected plan or order before the explicit confirmation action |
| Feedback | Alert, Empty and loading components with a next action; errors must not erase entered data |
| Help and supplementary information | Accordion for FAQs and Collapsible for delivery areas and staff preview tools |
| Staff lists | Table with scoped styles so operational tables do not affect the calendar grid |

The [shadcn Calendar](https://ui.shadcn.com/docs/components/base/calendar) and
[Slider](https://ui.shadcn.com/docs/components/base/slider) provide the reusable
control foundations. Gather owns the availability, pricing, validation and copy.

## Verification and remaining work

Browser journeys in `tests/browser/experience.spec.ts` and
`tests/browser/controls.spec.ts` cover navigation, selection repair, calendar
alignment, keyboard control, localization and consent changes. Domain
tests cover the serving bounds and box readiness. The existing purchase,
ownership, payment recovery, country and account journeys remain required.

Saved-box journeys in `tests/browser/drafts.spec.ts` cover signed guest
ownership, account attachment and conflicting device edits. Controller tests
exercise delayed loads, retries and changing identities. Motion behavior and
state preservation are exercised in `tests/browser/transitions.spec.ts`.
Cooking journeys in `tests/browser/cooking.spec.ts` exercise packed quantities,
unit choices, timer navigation and feedback ownership, replay and conflicts.
Pure timer and quantity rules are covered by `tests/cooking.test.ts`.
Future boxes still require explicit confirmation; automatic recurrence,
provider payments and operational integrations remain separate work in the
implementation ledger.
