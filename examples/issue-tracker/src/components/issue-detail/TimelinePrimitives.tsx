import type { HTMLAttributes, ReactNode } from "react";

export function TimelineList({
  kind,
  children,
}: {
  kind: "comments" | "history";
  children: ReactNode;
}) {
  const Component = kind === "comments" ? "ul" : "ol";

  return (
    <Component
      className={`${kind === "comments" ? "comment-list mb-5" : "history-timeline"} relative m-0 flex list-none flex-col gap-5 p-0 before:absolute before:top-6 before:bottom-2 before:start-4 before:w-px before:bg-line before:content-['']`}
    >
      {children}
    </Component>
  );
}

export function TimelineRow({
  kind,
  isPrivate = false,
  isDescription = false,
  children,
  ...props
}: Omit<HTMLAttributes<HTMLLIElement>, "className"> & {
  kind: "comment" | "activity" | "history";
  isPrivate?: boolean;
  isDescription?: boolean;
}) {
  const locator =
    kind === "comment" ? "comment" : kind === "activity" ? "timeline-event" : "history-event";

  return (
    <li
      {...props}
      className={`${locator} ${kind === "comment" ? "group/comment" : ""} ${isPrivate ? "private" : ""} ${isDescription ? "is-description" : ""} relative grid grid-cols-[2rem_minmax(0,1fr)] items-start gap-3 [&>*:first-child]:relative [&>*:first-child]:z-10`}
    >
      {children}
    </li>
  );
}

export function TimelineBody({
  tone = "plain",
  children,
}: {
  tone?: "plain" | "description" | "private";
  children: ReactNode;
}) {
  return (
    <div
      className={`min-w-0 ${
        tone === "description"
          ? "rounded-e-lg border-s-2 border-accent bg-surface-sunken px-3 py-2"
          : tone === "private"
            ? "rounded-e-lg border-s-2 border-danger bg-danger-soft px-3 py-2"
            : ""
      }`}
    >
      {children}
    </div>
  );
}
