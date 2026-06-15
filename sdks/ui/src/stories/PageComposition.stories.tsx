import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import {
  ResponsivePicture,
  ScrollRail,
  SectionFrame,
  SectionHeader,
} from "../sections";
import { Badge, Button, Card } from "../components";

const productSvg = (label: string, color: string) =>
  `data:image/svg+xml;utf8,${encodeURIComponent(`
    <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 480 384">
      <rect width="480" height="384" fill="#f5f5f7"/>
      <circle cx="240" cy="162" r="96" fill="${color}" opacity="0.9"/>
      <rect x="156" y="270" width="168" height="20" rx="10" fill="#1d1d1f" opacity="0.24"/>
      <text x="240" y="328" text-anchor="middle" font-family="-apple-system, BlinkMacSystemFont, sans-serif" font-size="28" font-weight="700" fill="#1d1d1f">${label}</text>
    </svg>
  `)}`;

const products = [
  {
    name: "Creator phone",
    copy: "A premium product tile with a fixed media crop.",
    color: "#7cc7ff",
  },
  {
    name: "Launch watch",
    copy: "Scroll rails keep cards aligned to the page width.",
    color: "#a7d36d",
  },
  {
    name: "Deploy tag",
    copy: "ResponsivePicture owns aspect ratio and crop behavior.",
    color: "#ffb86b",
  },
  {
    name: "Cloud buds",
    copy: "The card surface stays boring; the media does the work.",
    color: "#c5b7ff",
  },
];

const meta: Meta = {
  title: "Sections/Page Composition",
  parameters: {
    layout: "fullscreen",
    docs: {
      description: {
        component:
          "Page-composition primitives learned from the Apple clone: a governed section frame, split section header, horizontal scroll rail, and responsive media crop.",
      },
    },
  },
};

export default meta;

type Story = StoryObj;

export const ProductRail: Story = {
  name: "Product rail (SectionFrame + SectionHeader + ScrollRail)",
  render: () => (
    <SectionFrame
      data-testid="composition-section"
      tone="muted"
      size="wide"
      header={
        <SectionHeader
          eyebrow={<Badge intent="info">New composition layer</Badge>}
          title="Build polished product pages from governed pieces."
          description="Use one width authority, one header rhythm, and one media crop contract instead of rebuilding section CSS for every generated page."
          actions={
            <Button variant="plain" size="small">
              View all
            </Button>
          }
        />
      }
    >
      <ScrollRail data-testid="composition-rail" aria-label="Product examples">
        {products.map((product) => (
          <Card
            key={product.name}
            variant="surface"
            style={{ inlineSize: "18rem", flex: "0 0 18rem" }}
          >
            <Card.Media side="top">
              <ResponsivePicture
                src={productSvg(product.name, product.color)}
                alt=""
                ratio="product"
                fit="cover"
                frame="clip"
                radius="md"
                loading="lazy"
              />
            </Card.Media>
            <Card.Header>
              <Card.Title>{product.name}</Card.Title>
              <Card.Description>{product.copy}</Card.Description>
            </Card.Header>
          </Card>
        ))}
      </ScrollRail>
    </SectionFrame>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const section = canvas.getByTestId("composition-section");
    await expect(section.tagName).toBe("SECTION");
    await expect(section).toHaveAttribute("data-section-band");
    await expect(section).toHaveAttribute("data-tone", "muted");
    await expect(section).toHaveAttribute("data-spacing", "regular");

    const heading = canvas.getByRole("heading", {
      name: /build polished product pages/i,
    });
    await expect(heading.tagName).toBe("H2");
    await expect(heading).toHaveAttribute("data-slot", "section-header-title");

    const rail = canvas.getByTestId("composition-rail");
    await expect(rail).toHaveAttribute("data-slot", "scroll-rail");
    await expect(rail).toHaveAttribute("data-size", "wide");
    await expect(rail.children.length).toBe(products.length);

    const pictures = rail.querySelectorAll("[data-slot='responsive-picture']");
    await expect(pictures.length).toBe(products.length);
    await expect(pictures[0]).toHaveAttribute("data-ratio", "product");
    await expect(pictures[0]).toHaveAttribute("data-fit", "cover");
  },
};

export const ResponsiveMedia: Story = {
  name: "Responsive media crop",
  render: () => (
    <SectionFrame
      data-testid="media-section"
      spacing="compact"
      header={
        <SectionHeader
          title="Media crops are design-system behavior now."
          titleSize="large"
          description="Breakpoint sources, fit, position, radius, and surface framing live behind one component."
        />
      }
    >
      <ResponsivePicture
        src={productSvg("Hero media", "#7cc7ff")}
        alt="Abstract product preview"
        ratio="banner"
        fit="cover"
        position="center"
        frame="surface"
        radius="xl"
        sources={[
          {
            media: "(max-width: 47.999rem)",
            srcSet: productSvg("Mobile media", "#ffb86b"),
          },
        ]}
      />
    </SectionFrame>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const picture = canvasElement.querySelector(
      "[data-slot='responsive-picture']",
    );
    await expect(picture).not.toBeNull();
    if (picture == null) throw new Error("responsive picture not found");
    await expect(picture.tagName).toBe("PICTURE");
    await expect(picture).toHaveAttribute("data-frame", "surface");
    await expect(picture).toHaveAttribute("data-radius", "xl");
    await expect(picture.querySelectorAll("source").length).toBe(1);
    const image = canvas.getByAltText("Abstract product preview");
    await expect(image.tagName).toBe("IMG");
  },
};
