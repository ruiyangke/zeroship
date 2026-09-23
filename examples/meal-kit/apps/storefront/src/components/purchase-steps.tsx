import { Link } from "react-router-dom";
import { msg } from "@lingui/core/macro";
import { useLingui } from "@lingui/react";
import { useGather } from "../state";
import { Check } from "lucide-react";
import { deliveryEligible } from "@gather/meal-kit/countries";

export function PurchaseSteps({
  current,
}: {
  current: "delivery" | "plan" | "menu" | "box" | "checkout";
}) {
  const { path, cart, market } = useGather();
  const { _: t } = useLingui();
  const steps = [
    ["delivery", "/plans", t(msg`Delivery area`)],
    ["plan", "/plans?step=box", t(msg`Box size`)],
    ["menu", "/menu", t(msg`Meals`)],
    ["box", "/box", t(msg`Review`)],
    ["checkout", "/checkout", t(msg`Checkout`)],
  ];
  const active = steps.findIndex(([route]) => route === current);
  return (
    <nav aria-label={t(msg`Build your box`)} className="purchase-steps">
      <ol>
        {steps.map(([step, route, label], index) => (
          <li key={step} aria-current={step === current ? "step" : undefined}>
            {index < active ? (
              <Link to={path(route)}>
                <span className="progress-dot" aria-hidden="true">
                  {deliveryEligible(market, cart) ? (
                    <Check size={16} />
                  ) : (
                    index + 1
                  )}
                </span>
                <span>{label}</span>
              </Link>
            ) : (
              <span>
                <span className="progress-dot" aria-hidden="true">
                  {index + 1}
                </span>
                <span>{label}</span>
              </span>
            )}
          </li>
        ))}
      </ol>
    </nav>
  );
}
