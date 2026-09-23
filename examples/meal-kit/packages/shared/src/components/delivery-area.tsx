import { SelectItem } from "@gather/meal-kit/components/select-field";
import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import { chinaDistricts, type DeliveryArea } from "@gather/meal-kit/countries";
import { Field, Select } from "@gather/meal-kit/components/shared";

export function DeliveryAreaFields({
  value,
  onChange,
  errorField,
  error,
}: {
  value: DeliveryArea;
  onChange: (value: DeliveryArea) => void;
  errorField?: string;
  error?: string;
}) {
  const { _: t } = useLingui();
  return (
    <div className="grid gap-x-5 sm:grid-cols-3">
      <Field
        label={t(msg`Province or municipality`)}
        error={errorField === "province" ? error : undefined}
      >
        <Select
          required
          name="province"
          autoComplete="address-level1"
          value={value.province}
          onValueChange={(selectedValue) =>
            onChange({ province: selectedValue, city: "", district: "" })
          }
        >
          <SelectItem value="">{t(msg`Select province`)}</SelectItem>
          <SelectItem value="shanghai">{t(msg`Shanghai`)}</SelectItem>
          <SelectItem value="other">
            {t(msg`Other province or municipality`)}
          </SelectItem>
        </Select>
      </Field>
      <Field
        label={t(msg`City`)}
        error={errorField === "city" ? error : undefined}
      >
        <Select
          required
          name="city"
          autoComplete="address-level2"
          value={value.city}
          disabled={!value.province}
          onValueChange={(selectedValue) =>
            onChange({ ...value, city: selectedValue, district: "" })
          }
        >
          <SelectItem value="">{t(msg`Select city`)}</SelectItem>
          {value.province === "shanghai" && (
            <SelectItem value="shanghai">{t(msg`Shanghai`)}</SelectItem>
          )}
          <SelectItem value="other">{t(msg`Other city`)}</SelectItem>
        </Select>
      </Field>
      <Field
        label={t(msg`District`)}
        error={errorField === "district" ? error : undefined}
      >
        <Select
          required
          name="district"
          autoComplete="address-level3"
          value={value.district}
          disabled={!value.city}
          onValueChange={(selectedValue) =>
            onChange({ ...value, district: selectedValue })
          }
        >
          <SelectItem value="">{t(msg`Select district`)}</SelectItem>
          {value.city === "shanghai" &&
            Object.entries(chinaDistricts).map(([id, label]) => (
              <SelectItem key={id} value={id}>
                {t(label)}
              </SelectItem>
            ))}
          <SelectItem value="other">{t(msg`Other district`)}</SelectItem>
        </Select>
      </Field>
    </div>
  );
}
