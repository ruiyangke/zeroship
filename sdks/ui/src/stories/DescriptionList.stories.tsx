import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import { DescriptionList } from "../blocks";

const meta: Meta<typeof DescriptionList> = {
  title: "Blocks/DescriptionList",
  component: DescriptionList,
  parameters: { layout: "fullscreen" },
  argTypes: {
    orientation: {
      control: "inline-radio",
      options: ["horizontal", "vertical"],
    },
    divider: { control: "boolean" },
  },
};

export default meta;

type Story = StoryObj<typeof DescriptionList>;

const ROWS: Array<[string, string]> = [
  ["Status", "Active"],
  ["Plan", "Pro (annual)"],
  ["Seats", "12 of 25"],
  ["Created", "May 12, 2026"],
];

function Rows() {
  return (
    <>
      {ROWS.map(([term, detail]) => (
        <DescriptionList.Item key={term}>
          <DescriptionList.Term>{term}</DescriptionList.Term>
          <DescriptionList.Detail>{detail}</DescriptionList.Detail>
        </DescriptionList.Item>
      ))}
    </>
  );
}

/* ─── 1. Horizontal (default) ────────────────────────────────────────── */
export const Horizontal: Story = {
  name: "Horizontal (default)",
  parameters: {
    docs: {
      description: {
        story:
          "Default `orientation='horizontal'`: each item is a two-column " +
          "grid — the term in a fixed-ish first track, the detail filling " +
          "the rest. Built on the native `<dl>` / `<dt>` / `<dd>` " +
          "elements; the play() asserts that structure.",
      },
    },
  },
  render: () => (
    <DescriptionList data-testid="dl-horizontal">
      <Rows />
    </DescriptionList>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("dl-horizontal");
    // Semantic structure: a <dl> root with dt/dd pairs.
    await expect(root.tagName).toBe("DL");
    await expect(root).toHaveAttribute("data-orientation", "horizontal");

    const terms = root.querySelectorAll("dt");
    const details = root.querySelectorAll("dd");
    await expect(terms).toHaveLength(ROWS.length);
    await expect(details).toHaveLength(ROWS.length);

    // Each pair is grouped in a <div> item (valid dl grouping).
    const firstItem = root.querySelector('[data-slot="description-list-item"]');
    await expect(firstItem?.tagName).toBe("DIV");
    await expect(firstItem?.querySelector("dt")).toHaveTextContent("Status");
    await expect(firstItem?.querySelector("dd")).toHaveTextContent("Active");
  },
};

/* ─── 2. Vertical ────────────────────────────────────────────────────── */
export const Vertical: Story = {
  name: "Vertical",
  parameters: {
    docs: {
      description: {
        story:
          "`orientation='vertical'` stacks the term (a small caption) " +
          "above the detail value.",
      },
    },
  },
  render: () => (
    <DescriptionList orientation="vertical">
      <Rows />
    </DescriptionList>
  ),
};

/* ─── 3. With divider ────────────────────────────────────────────────── */
export const WithDivider: Story = {
  name: "With divider",
  parameters: {
    docs: {
      description: {
        story:
          "`divider` draws a `--zs-separator` hairline between items " +
          "(a logical block-start border) so a long list reads as " +
          "discrete rows.",
      },
    },
  },
  render: () => (
    <DescriptionList divider>
      <Rows />
    </DescriptionList>
  ),
};
