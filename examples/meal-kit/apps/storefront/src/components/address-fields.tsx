import { Textarea } from "@gather/meal-kit/components/ui/textarea";
import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import { useGather } from "../state";
import type { Address } from "@gather/meal-kit/domain";
import { countryPolicies } from "@gather/meal-kit/countries";
import { DeliveryAreaFields } from "@gather/meal-kit/components/delivery-area";
import { Field, Input } from "@gather/meal-kit/components/shared";

export function AddressFields({
  value,
  onChange,
  errorField,
  error,
}: {
  value: Address;
  onChange: (address: Address) => void;
  errorField?: string;
  error?: string;
}) {
  const { _: t } = useLingui();
  const { market } = useGather();
  const china = value.country === "CN";
  const fields = [
    ["name", t(msg`Recipient name`), "name"],
    ...(!china ? [["email", t(msg`Email address`), "email"]] : []),
    [
      "line",
      china ? t(msg`Street, building and apartment`) : t(msg`Street address`),
      "street-address",
    ],
    ...(!china ? [["city", t(msg`City`), "address-level2"]] : []),
    ...(value.country === "US"
      ? [["province", t(msg`State`), "address-level1"]]
      : []),
    ...(!china
      ? [["postal", t(countryPolicies[market].postalLabel), "postal-code"]]
      : []),
    ["phone", china ? t(msg`Mobile number`) : t(msg`Phone number`), "tel"],
  ];
  return (
    <>
      {china && (
        <DeliveryAreaFields
          errorField={errorField}
          error={error}
          value={{
            province: value.province,
            city: value.city,
            district: value.district,
          }}
          onChange={(area) => onChange({ ...value, ...area })}
        />
      )}
      <div className="form-grid">
        {fields.map(([key, label, complete]) => (
          <Field
            key={key}
            label={label}
            error={errorField === key ? error : undefined}
          >
            <Input
              name={key}
              required
              type={
                key === "email" ? "email" : key === "phone" ? "tel" : "text"
              }
              autoComplete={complete}
              inputMode={
                key === "postal" && value.country === "US"
                  ? "numeric"
                  : undefined
              }
              autoCapitalize={
                key === "email"
                  ? "none"
                  : key === "postal"
                    ? "characters"
                    : undefined
              }
              spellCheck={
                key === "email" || key === "postal" || key === "phone"
                  ? false
                  : undefined
              }
              maxLength={
                key === "line" || key === "email"
                  ? 200
                  : key === "phone"
                    ? 30
                    : key === "postal"
                      ? 20
                      : 100
              }
              value={value[key as keyof Address]}
              onChange={(event) =>
                onChange({ ...value, [key]: event.target.value })
              }
            />
          </Field>
        ))}
        <div className="col-span-full">
          <Field label={t(msg`Delivery instructions (optional)`)}>
            <Textarea
              rows={2}
              name="instructions"
              value={value.instructions}
              maxLength={500}
              onChange={(event) =>
                onChange({ ...value, instructions: event.target.value })
              }
            />
          </Field>
        </div>
      </div>
    </>
  );
}
