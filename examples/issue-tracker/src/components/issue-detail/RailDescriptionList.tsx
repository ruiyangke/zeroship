import type { ReactNode } from "react";

/** A rail-native description list that participates in the parent subgrid. */
export function RailDescriptionList({
  items,
}: {
  items: { term: ReactNode; detail: ReactNode }[];
}) {
  return (
    <dl className="col-span-full grid grid-cols-subgrid text-ink">
      {items.map((item, index) => (
        <div
          className="col-span-full grid min-h-8 grid-cols-subgrid items-center gap-x-2! py-1"
          data-slot="description-list-item"
          key={index}
        >
          <dt
            className="text-xs font-medium tracking-wide text-ink-muted"
            data-slot="description-list-term"
          >
            {item.term}
          </dt>
          <dd className="min-w-0 text-ink" data-slot="description-list-detail">
            {item.detail}
          </dd>
        </div>
      ))}
    </dl>
  );
}
