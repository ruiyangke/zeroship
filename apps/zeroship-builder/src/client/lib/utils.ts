import { type ClassValue, clsx } from "clsx";

// Class-name joiner. With Tailwind retired (the app styles via @zeroship/ui
// + co-located --zs-* CSS), there are no utility-class conflicts to dedupe,
// so clsx alone suffices — no tailwind-merge.
export function cn(...inputs: ClassValue[]) {
  return clsx(inputs);
}
