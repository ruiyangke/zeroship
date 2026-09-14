import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, within, waitFor } from "@storybook/test";
import { Faq, type FaqEntry } from "../sections";

const meta: Meta<typeof Faq> = {
  title: "Sections/Faq",
  component: Faq,
  parameters: { layout: "fullscreen" },
  argTypes: {
    tone: {
      control: "inline-radio",
      options: ["default", "muted", "accent"],
      description:
        "Full-bleed band tone (the shared page-rhythm system): default " +
        "(transparent), muted (subtle surface panel), accent (accent fill " +
        "with ink remapped to accent-ink).",
    },
  },
};

export default meta;

type Story = StoryObj<typeof Faq>;

const fourQuestions: FaqEntry[] = [
  {
    id: "cost",
    question: "How much does it cost?",
    answer:
      "The platform takes 15% of what your app earns; you keep the rest. " +
      "No upfront fees, no per-seat pricing.",
  },
  {
    id: "code",
    question: "Do I need to write code?",
    answer: "No — describe what you want in natural language and AI builds it.",
  },
  {
    id: "host",
    question: "Who handles hosting?",
    answer: "We do — hosting, database, auth, payments, and scaling are managed.",
  },
  {
    id: "own",
    question: "Do I own my app?",
    answer: "Yes. You can export and take your code with you at any time.",
  },
];

/* ─── 1. Default — header + 4 Q/A, single-open ───────────────────────────── */
export const Default: Story = {
  name: "Default (header + 4 Q/A, single-open)",
  args: {
    tone: "default",
  },
  parameters: {
    docs: {
      description: {
        story:
          "The default band: a `<h2>` title above a composed `Accordion` of " +
          "four question→answer rows, single-open (one answer at a time, " +
          "collapsible). Each question is an `<h3>` heading-button; each " +
          "answer is a labelled region. Clicking a question expands its " +
          "answer (the real Accordion behavior).",
      },
    },
  },
  render: (args) => (
    <Faq
      {...args}
      data-testid="faq-default"
      eyebrow="Help"
      title="Frequently asked questions"
      description="Everything you need to know before you start."
      items={fourQuestions}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("faq-default");
    await expect(root.tagName).toBe("SECTION");
    await expect(root).toHaveAttribute("data-slot", "faq");

    // Labelled by the real <h2>.
    const title = canvas.getByRole("heading", {
      name: "Frequently asked questions",
    });
    await expect(title.tagName).toBe("H2");
    await expect(root.getAttribute("aria-labelledby")).toBe(title.id);

    // The questions are <h3> heading-buttons (the composed Accordion).
    const trigger = canvas.getByRole("button", {
      name: "How much does it cost?",
    });
    await expect(trigger).toHaveAttribute("aria-expanded", "false");

    // Clicking the question expands its answer — the REAL Accordion behavior.
    await userEvent.click(trigger);
    await waitFor(async () => {
      await expect(trigger).toHaveAttribute("aria-expanded", "true");
    });
    await expect(
      canvas.getByText(/The platform takes 15%/),
    ).toBeVisible();
  },
};

/* ─── 2. Multiple — openMultiple ─────────────────────────────────────────── */
export const Multiple: Story = {
  name: "Multiple (multiple open at once)",
  args: {
    tone: "default",
  },
  parameters: {
    docs: {
      description: {
        story:
          "`multiple` maps to the composed Accordion's `multiple` mode — any " +
          "number of answers can be open at once.",
      },
    },
  },
  render: (args) => (
    <Faq
      {...args}
      data-testid="faq-multiple"
      title="FAQ"
      multiple
      items={fourQuestions}
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const cost = canvas.getByRole("button", { name: "How much does it cost?" });
    const code = canvas.getByRole("button", { name: "Do I need to write code?" });

    // Open both — multiple mode keeps both expanded simultaneously.
    await userEvent.click(cost);
    await userEvent.click(code);
    await waitFor(async () => {
      await expect(cost).toHaveAttribute("aria-expanded", "true");
      await expect(code).toHaveAttribute("aria-expanded", "true");
    });
  },
};

/* ─── 3. Compound — items prop + Faq.Item additive ───────────────────────── */
export const Compound: Story = {
  name: "Compound (items prop + Faq.Item, additive)",
  args: {
    tone: "default",
  },
  parameters: {
    docs: {
      description: {
        story:
          "The compound surface: one `items`-prop entry renders FIRST, then " +
          "the compound `<Faq.Item>` parts fall through after it (ADDITIVE — " +
          "no suppression). Questions render in order.",
      },
    },
  },
  render: (args) => (
    <Faq
      {...args}
      data-testid="faq-compound"
      title="Questions"
      items={[
        {
          id: "cost",
          question: "How much does it cost?",
          answer: "The platform takes 15%; you keep the rest.",
        },
      ]}
    >
      <Faq.Item question="Do I need to write code?">
        No — describe what you want and AI builds it.
      </Faq.Item>
      <Faq.Item
        question="Who handles hosting?"
        answer="We do — hosting, auth, and scaling are managed."
      />
    </Faq>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // Three question triggers: prop entry FIRST, then the two compound ones.
    const triggers = canvas.getAllByRole("button");
    const names = triggers.map((t) => t.textContent);
    await expect(names).toEqual([
      "How much does it cost?",
      "Do I need to write code?",
      "Who handles hosting?",
    ]);

    // The compound entry's children-as-answer expands on click.
    const second = canvas.getByRole("button", {
      name: "Do I need to write code?",
    });
    await userEvent.click(second);
    await waitFor(async () => {
      await expect(second).toHaveAttribute("aria-expanded", "true");
    });
    await expect(
      canvas.getByText(/describe what you want and AI builds it/),
    ).toBeVisible();
  },
};

/* ─── 4. Headerless — no header → no aria-labelledby ─────────────────────── */
export const Headerless: Story = {
  name: "Headerless (no header → no aria-labelledby)",
  args: {
    tone: "default",
  },
  parameters: {
    docs: {
      description: {
        story:
          "Used headerless, the `<section>` carries NO `aria-labelledby` — " +
          "the attr is gated on a real `<h2>` title rendering, so there is " +
          "never a dangling label reference. The composed Accordion still " +
          "provides the disclosure a11y.",
      },
    },
  },
  render: (args) => (
    <Faq {...args} data-testid="faq-headerless" items={fourQuestions} />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("faq-headerless");
    await expect(root.hasAttribute("aria-labelledby")).toBe(false);
    // No section <h2>; the questions are still <h3> heading-buttons.
    await expect(canvas.queryAllByRole("heading", { level: 2 }).length).toBe(0);
    await expect(
      canvas.getAllByRole("heading", { level: 3 }).length,
    ).toBe(4);
  },
};
