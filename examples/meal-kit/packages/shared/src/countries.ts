import type { MarketId } from "@gather/meal-kit/catalog";

export type DeliveryArea = { province: string; city: string; district: string };
export type Destination = { postal: string; area: DeliveryArea };
export const emptyArea = (): DeliveryArea => ({
  province: "",
  city: "",
  district: "",
});
export const chinaDistricts = {
  huangpu: /* i18n */ "Huangpu",
  xuhui: /* i18n */ "Xuhui",
  changning: /* i18n */ "Changning",
  jingan: /* i18n */ "Jing'an",
  pudong: /* i18n */ "Pudong",
} as const;
export const countryPolicies = {
  us: {
    addressKind: "postal",
    postalLabel: /* i18n */ "ZIP code",
    weekdays: [2, 5],
    cutoffDays: 2,
    cutoffHour: 18,
    holidays: [] as string[],
    version: "us-demo-v1",
  },
  uk: {
    addressKind: "postal",
    postalLabel: /* i18n */ "Postcode",
    weekdays: [3, 6],
    cutoffDays: 2,
    cutoffHour: 18,
    holidays: [] as string[],
    version: "uk-demo-v1",
  },
  cn: {
    addressKind: "administrative",
    postalLabel: /* i18n */ "Postal code (optional)",
    weekdays: [2, 4, 6],
    cutoffDays: 2,
    cutoffHour: 18,
    holidays: [] as string[],
    version: "cn-demo-v1",
  },
} as const;

export function deliveryEligible(
  market: MarketId,
  destination: Destination,
): boolean {
  if (market === "cn")
    return (
      destination.area.province === "shanghai" &&
      destination.area.city === "shanghai" &&
      Object.hasOwn(chinaDistricts, destination.area.district)
    );
  const postal = destination.postal.toUpperCase().replace(/\s/g, "");
  return market === "us"
    ? /^10\d{3}$/.test(postal)
    : /^(SW|W|E|N|SE|NW|EC|WC)\d[A-Z\d]*\d[A-Z]{2}$/.test(postal);
}
export function destinationKey(market: MarketId, destination: Destination) {
  return market === "cn"
    ? [
        destination.area.province,
        destination.area.city,
        destination.area.district,
      ].join(":")
    : destination.postal.toUpperCase().replace(/\s/g, "");
}
