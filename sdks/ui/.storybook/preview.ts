import type { Preview } from "@storybook/react";
import { createElement } from "react";
import { withThemeByDataAttribute } from "@storybook/addon-themes";
import "../src/styles.css";
import "../src/stories/story.css";

const preview: Preview = {
  decorators: [
    withThemeByDataAttribute({
      themes: {
        Atelier: "atelier",
        Studio: "studio",
        Dusk: "dusk",
      },
      defaultTheme: "atelier",
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
    layout: "centered",
  },
};

export default preview;
