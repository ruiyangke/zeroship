import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import { chinaDistricts } from "@gather/meal-kit/countries";
import type { Address } from "@gather/meal-kit/domain";

export function useAddressArea() {
  const { _: t } = useLingui();
  return (address: Address) =>
    address.country === "CN"
      ? `${address.city === "shanghai" ? t(msg`Shanghai`) : address.city} ${Object.hasOwn(chinaDistricts, address.district) ? t(chinaDistricts[address.district as keyof typeof chinaDistricts]) : address.district}`
      : [address.city, address.province, address.postal]
          .filter(Boolean)
          .join(" ");
}
export function DeliveryAddress({ address }: { address: Address }) {
  const area = useAddressArea();
  return (
    <address className="not-italic text-sm leading-7">
      <p>{address.name}</p>
      <p>{address.line}</p>
      <p>{area(address)}</p>
      <p>{address.phone}</p>
    </address>
  );
}
