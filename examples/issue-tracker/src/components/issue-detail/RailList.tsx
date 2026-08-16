import type { ReactNode } from "react";

/** A compact, marker-free list for the relation editors opened in the rail. */
export function RailList({
  children,
  rows = "plain",
  roomy = false,
}: {
  children: ReactNode;
  rows?: "plain" | "actions";
  /** Use the regular 8px rhythm for controls; relation text stays compact. */
  roomy?: boolean;
}) {
  return (
    <ul
      className={`m-0 flex list-none flex-col p-0 ${roomy ? "gap-2" : "gap-1"} ${
        rows === "actions" ? "[&>li]:flex [&>li]:items-center [&>li]:gap-2" : ""
      }`}
    >
      {children}
    </ul>
  );
}
