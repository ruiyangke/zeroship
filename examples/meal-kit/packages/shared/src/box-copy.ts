import { msg, plural } from "@lingui/core/macro";

export function boxSizeMessage(meals: number, people: number) {
  return msg`${meals} meals for ${plural(people, { one: "# person", other: "# people" })}`;
}
