import { twMerge, type ClassNameValue } from "tailwind-merge";

export function cn(...classNames: ClassNameValue[]) {
  return twMerge(...classNames);
}
