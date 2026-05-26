import type { Preview } from "@storybook/react";
import { createElement } from "react";
import { withThemeByDataAttribute } from "@storybook/addon-themes";
import "../src/styles.css";
import "../src/stories/story.css";

const preview: Preview = {
  decorators: [
    withThemeByDataAttribute({
      themes: {
        Studio: "studio",
        Atelier: "atelier",
        Dusk: "dusk",
        "Glass Dark": "glass-dark",
        "Glass Light": "glass-light",
      },
      defaultTheme: "Studio",
      attributeName: "data-theme",
      parentSelector: "html",
    }),
    (Story, context) =>
      createElement(
        "main",
        {
          className:
            context.viewMode === "docs"
              ? "zs-story-main zs-story-main--docs"
              : "zs-story-main",
          "aria-label": context.title,
        },
        createElement("h1", { className: "zs-sr-only" }, context.title),
        createElement(Story),
      ),
  ],
  parameters: {
    a11y: {
      element: "#storybook-root",
      config: {
        rules: [
          {
            id: "color-contrast",
            enabled: true,
          },
        ],
      },
    },
    controls: {
      matchers: {
        color: /(background|color)$/i,
        date: /Date$/i,
      },
    },
    docs: {
      toc: true,
    },
    // The story decorator (.zs-story-main) is full-bleed by design — it paints the
    // theme surface and centers content itself (min-height:100vh, place-items:center).
    // "fullscreen" lets it fill the canvas; "centered" shrink-wrapped it to content
    // width, leaving the themed area a narrow strip with large blank gutters.
    layout: "fullscreen",
  },
};

export default preview;
