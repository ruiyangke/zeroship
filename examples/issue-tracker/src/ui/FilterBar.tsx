import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
  type ReactNode,
} from "react";

import { Button } from "./Button";
import { Input } from "./Input";

export interface FilterBarActiveFilter {
  id: string;
  label: ReactNode;
  onRemove?: () => void;
  removeLabel?: string;
}

export interface FilterBarProps extends ComponentPropsWithoutRef<"div"> {
  search?: string;
  onSearchChange?: (value: string) => void;
  searchPlaceholder?: string;
  searchLabel?: string;
  activeFilters?: readonly FilterBarActiveFilter[];
  onClearFilters?: () => void;
  clearLabel?: ReactNode;
  actions?: ReactNode;
}

function SearchIcon() {
  return (
    <svg
      aria-hidden="true"
      className="size-4"
      fill="none"
      focusable="false"
      stroke="currentColor"
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth="2"
      viewBox="0 0 24 24"
    >
      <circle cx="11" cy="11" r="8" />
      <path d="m21 21-4.3-4.3" />
    </svg>
  );
}

function ActiveFilter({ filter }: { filter: FilterBarActiveFilter }) {
  const removeLabel =
    filter.removeLabel ??
    (typeof filter.label === "string" ? `Remove ${filter.label}` : "Remove");
  const remove = (
    event:
      | ReactMouseEvent<HTMLButtonElement>
      | ReactKeyboardEvent<HTMLButtonElement>,
  ) => {
    event.preventDefault();
    event.stopPropagation();
    filter.onRemove?.();
  };

  return (
    <span className="inline-flex h-7 min-w-0 items-center gap-1 whitespace-nowrap rounded border border-line bg-surface-sunken ps-2 pe-1 font-sans text-sm font-medium leading-tight text-ink-secondary">
      <span className="min-w-0 truncate">{filter.label}</span>
      <button
        type="button"
        aria-label={removeLabel}
        className="inline-flex size-4 flex-none cursor-pointer items-center justify-center rounded-sm border-0 bg-transparent p-0 text-ink-muted hover:bg-danger-soft hover:text-danger focus-visible:focus-ring-tight"
        onClick={remove}
        onKeyDown={(event) => {
          if (event.key === "Backspace" || event.key === "Delete") remove(event);
        }}
      >
        <span aria-hidden="true" className="leading-tight">
          ×
        </span>
      </button>
    </span>
  );
}

export const FilterBar = forwardRef<HTMLDivElement, FilterBarProps>(function FilterBar(
  {
    search,
    onSearchChange,
    searchPlaceholder = "Search…",
    searchLabel,
    activeFilters = [],
    onClearFilters,
    clearLabel = "Clear",
    actions,
    className,
    children,
    ...props
  },
  ref,
) {
  const searchable = search !== undefined || onSearchChange !== undefined;

  return (
    <div
      {...props}
      ref={ref}
      role={searchable ? "search" : undefined}
      className={`flex min-h-8 w-full flex-wrap items-center gap-2 border-b border-line py-1${
        className ? ` ${className}` : ""
      }`}
    >
      {searchable ? (
        <Input
          type="search"
          data-slot="filter-bar-search"
          wrapperClassName="min-w-12 grow shrink basis-12"
          value={search}
          onChange={(event) => onSearchChange?.(event.target.value)}
          placeholder={searchPlaceholder}
          aria-label={searchLabel ?? searchPlaceholder}
          startSlot={<SearchIcon />}
        />
      ) : null}

      {activeFilters.length > 0 ? (
        <div className="flex flex-none flex-wrap items-center gap-1">
          {activeFilters.map((filter) => (
            <ActiveFilter key={filter.id} filter={filter} />
          ))}
        </div>
      ) : null}

      {activeFilters.length > 0 && onClearFilters != null ? (
        <Button type="button" variant="plain" onClick={onClearFilters}>
          {clearLabel}
        </Button>
      ) : null}

      {children}
      <span aria-hidden="true" className="min-w-0 flex-1 basis-0" />
      {actions != null ? (
        <div className="ms-auto flex flex-none flex-wrap items-center gap-2">{actions}</div>
      ) : null}
    </div>
  );
});

FilterBar.displayName = "FilterBar";
