import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react";
import { expect, fn, userEvent, within } from "@storybook/test";
import { SlidersHorizontal } from "lucide-react";
import { FilterBar, type FilterBarActiveFilter } from "../blocks";
import { Button } from "../components/Button";
import { Icon } from "../components/Icon";

const meta: Meta<typeof FilterBar> = {
  title: "Blocks/FilterBar",
  component: FilterBar,
  parameters: { layout: "fullscreen" },
};

export default meta;

type Story = StoryObj<typeof FilterBar>;

/* A trailing actions Button used across the stories. */
function FiltersButton() {
  return (
    <Button variant="tinted" size="small">
      <Icon as={SlidersHorizontal} size="sm" />
      Filters
    </Button>
  );
}

/* ─── 1. Default — search + 2 chips + Clear + actions ────────────────── */
export const Default: Story = {
  name: "Default (search + chips + clear + actions)",
  parameters: {
    docs: {
      description: {
        story:
          "The full toolbar: a `role=\"search\"` landmark wrapping a search " +
          "Input (decorative Search Icon in its `startSlot`), two removable " +
          "filter Tags, a plain Clear Button (present because " +
          "`onClearFilters` is set and there are active filters), and a " +
          "trailing actions Button. The play() types into the search " +
          "(asserts `onSearchChange` fires), removes a chip (asserts that " +
          "chip's `onRemove` fires), and clicks Clear (asserts " +
          "`onClearFilters` fires).",
      },
    },
  },
  args: {
    searchPlaceholder: "Search projects…",
    onSearchChange: fn(),
    onClearFilters: fn(),
  },
  render: (args) => {
    // Controlled-search wrapper so typing is reflected back into the box
    // while still routing every keystroke through the spy.
    const [value, setValue] = useState("");
    const filters: FilterBarActiveFilter[] = [
      { id: "status", label: "Status: Active", onRemove: fn() },
      { id: "owner", label: "Owner: Me", onRemove: fn() },
    ];
    return (
      <div style={{ padding: "var(--zs-space-4)" }}>
        <FilterBar
          {...args}
          data-testid="filter-bar"
          search={value}
          onSearchChange={(next) => {
            setValue(next);
            args.onSearchChange?.(next);
          }}
          activeFilters={filters}
          actions={<FiltersButton />}
        />
      </div>
    );
  },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);

    // Root is a search landmark because the bar owns a search field.
    const root = canvas.getByTestId("filter-bar");
    await expect(root).toHaveAttribute("role", "search");

    // Type into the search field → onSearchChange fires per keystroke.
    const searchBox = canvas.getByRole("searchbox", {
      name: /search projects/i,
    });
    await userEvent.type(searchBox, "api");
    await expect(args.onSearchChange).toHaveBeenCalled();
    await expect(args.onSearchChange).toHaveBeenLastCalledWith("api");

    // Remove the first chip → its own onRemove fires (Tag's real remove
    // button, labelled "Remove Status: Active").
    const removeStatus = canvas.getByRole("button", {
      name: /remove status: active/i,
    });
    await userEvent.click(removeStatus);
    // The owner chip's remove button is unaffected.
    await expect(
      canvas.getByRole("button", { name: /remove owner: me/i }),
    ).toBeInTheDocument();

    // Click Clear → onClearFilters fires.
    const clear = canvas.getByRole("button", { name: /^clear$/i });
    await userEvent.click(clear);
    await expect(args.onClearFilters).toHaveBeenCalledTimes(1);
  },
};

/* ─── 2. SearchOnly — search field, no chips ─────────────────────────── */
export const SearchOnly: Story = {
  name: "Search only (no chips)",
  parameters: {
    docs: {
      description: {
        story:
          "Just a search field + actions. Still a `role=\"search\"` " +
          "landmark. No active filters → no Clear button (nothing to clear).",
      },
    },
  },
  args: { searchPlaceholder: "Filter results…", onSearchChange: fn() },
  render: (args) => {
    const [value, setValue] = useState("");
    return (
      <div style={{ padding: "var(--zs-space-4)" }}>
        <FilterBar
          {...args}
          data-testid="filter-bar"
          search={value}
          onSearchChange={(next) => {
            setValue(next);
            args.onSearchChange?.(next);
          }}
          actions={<FiltersButton />}
        />
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("filter-bar");
    await expect(root).toHaveAttribute("role", "search");
    // No Clear button when there are no active filters.
    await expect(
      canvas.queryByRole("button", { name: /^clear$/i }),
    ).toBeNull();
  },
};

/* ─── 3. FiltersOnly — chips + actions, NO search (no role) ──────────── */
export const FiltersOnly: Story = {
  name: "Filters only (no search → no role)",
  parameters: {
    docs: {
      description: {
        story:
          "A chips + actions bar with NO search field. Because there is no " +
          "search input, the root is a plain role-less group — claiming " +
          "`role=\"search\"` here would be a landmark lie. Clear shows " +
          "because `onClearFilters` is set with active filters present.",
      },
    },
  },
  args: { onClearFilters: fn() },
  render: (args) => {
    const filters: FilterBarActiveFilter[] = [
      { id: "type", label: "Type: Image", onRemove: fn() },
      { id: "size", label: "Size: Large", onRemove: fn() },
      { id: "color", label: "Color: Blue", onRemove: fn() },
    ];
    return (
      <div style={{ padding: "var(--zs-space-4)" }}>
        <FilterBar
          {...args}
          data-testid="filter-bar"
          activeFilters={filters}
          actions={<FiltersButton />}
        />
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("filter-bar");
    // No search field → no role at all.
    await expect(root).not.toHaveAttribute("role");
    await expect(
      canvas.queryByRole("searchbox"),
    ).toBeNull();
    // Three removable chips are present.
    await expect(
      canvas.getByRole("button", { name: /remove type: image/i }),
    ).toBeInTheDocument();
  },
};

/* ─── 4. Empty — no search, no filters, no actions ───────────────────── */
export const Empty: Story = {
  name: "Empty (role-less, nothing but the spacer)",
  parameters: {
    docs: {
      description: {
        story:
          "The degenerate case: no search, no filters, no actions. The bar " +
          "renders a role-less group with only its (invisible) spacer — no " +
          "Clear, no chips, no search field.",
      },
    },
  },
  render: () => (
    <div style={{ padding: "var(--zs-space-4)" }}>
      <FilterBar data-testid="filter-bar" />
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("filter-bar");
    await expect(root).not.toHaveAttribute("role");
    await expect(canvas.queryByRole("searchbox")).toBeNull();
    await expect(
      canvas.queryByRole("button", { name: /^clear$/i }),
    ).toBeNull();
  },
};
