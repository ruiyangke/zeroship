import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type MouseEventHandler,
  type ReactNode,
} from "react";

import { cn } from "./cn";

export interface ListViewItem {
  id: string;
  leading?: ReactNode;
  title: ReactNode;
  description?: ReactNode;
  meta?: ReactNode;
  trailing?: ReactNode;
  href?: string;
  onClick?: MouseEventHandler<HTMLButtonElement>;
}

export interface ListViewProps extends Omit<ComponentPropsWithoutRef<"ul">, "children"> {
  items: readonly ListViewItem[];
  divided?: boolean;
  density?: "comfortable" | "compact";
}

const MAIN_CLASSES =
  "flex min-h-8 min-w-0 flex-1 items-center gap-2 rounded border-0 bg-transparent px-0 py-1 text-left text-inherit no-underline hover:no-underline";

function itemMain(item: ListViewItem) {
  const contents = (
    <>
      {item.leading != null ? (
        <span className="inline-flex size-6 min-w-6 flex-none items-center justify-center">
          {item.leading}
        </span>
      ) : null}
      <span className="flex min-w-0 flex-col justify-center">
        <span className="truncate text-base font-medium leading-snug text-ink">
          {item.title}
        </span>
        {item.description != null ? (
          <span className="truncate text-xs leading-snug text-ink-secondary">
            {item.description}
          </span>
        ) : null}
      </span>
    </>
  );

  if (item.href != null) {
    return (
      <a className={cn(MAIN_CLASSES, "w-full cursor-pointer")} href={item.href}>
        {contents}
      </a>
    );
  }
  if (item.onClick != null) {
    return (
      <button
        type="button"
        className={cn(MAIN_CLASSES, "w-full cursor-pointer")}
        onClick={item.onClick}
      >
        {contents}
      </button>
    );
  }
  return <div className={MAIN_CLASSES}>{contents}</div>;
}

export const ListView = forwardRef<HTMLUListElement, ListViewProps>(function ListView(
  {
    items,
    divided = true,
    density = "comfortable",
    className,
    ...props
  },
  ref,
) {
  return (
    <ul
      {...props}
      ref={ref}
      className={cn(
        "m-0 w-full list-none rounded border border-line bg-surface p-0 text-ink",
        className,
      )}
    >
      {items.map((item, index) => (
        <li
          key={item.id}
          className={cn(
            "flex min-h-8 items-center duration-fast ease-out motion-reduce:transition-none",
            density === "compact" ? "px-2" : "px-3",
            divided && index > 0 && "border-t border-line",
            (item.href != null || item.onClick != null) &&
              "transition-colors hover:bg-surface-hover",
          )}
        >
          {itemMain(item)}
          {item.meta != null || item.trailing != null ? (
            <span className="inline-flex min-w-0 items-center justify-end gap-2 ps-3">
              {item.meta != null ? (
                <span className="truncate font-mono text-xs tabular-nums text-ink-muted">
                  {item.meta}
                </span>
              ) : null}
              {item.trailing != null ? (
                <span className="inline-flex flex-none items-center justify-end">
                  {item.trailing}
                </span>
              ) : null}
            </span>
          ) : null}
        </li>
      ))}
    </ul>
  );
});

ListView.displayName = "ListView";
