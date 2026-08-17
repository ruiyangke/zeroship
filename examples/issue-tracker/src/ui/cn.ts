import { extendTailwindMerge, type ClassNameValue } from "tailwind-merge";

const mergeClasses = extendTailwindMerge({
  extend: {
    theme: {
      shadow: ["popup", "dialog"],
    },
    classGroups: {
      content: [{ content: ["empty", "placeholder"] }],
      duration: [{ duration: ["fast", "base"] }],
      shadow: ["field-edge", "field-edge-focus", "focus-ring-tight"],
    },
  },
});

export function cn(...classNames: ClassNameValue[]) {
  return mergeClasses(...classNames);
}
