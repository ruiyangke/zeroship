import type { Preview } from "@storybook/react";
import { createElement } from "react";
import { withThemeByDataAttribute } from "@storybook/addon-themes";
import "../src/styles.css";
import "../src/stories/story.css";

const preview: Preview = {
  initialGlobals: {
    theme: "Crystal Light",
  },
  decorators: [
    withThemeByDataAttribute({
      themes: {
        "Crystal Light": "crystal-light",
        "Crystal Dark": "crystal-dark",
        "Studio Light": "studio-light",
        "Ghibli Light": "ghibli-light",
      },
      defaultTheme: "Crystal Light",
      attributeName: "data-theme",
      parentSelector: "html",
    }),
    (Story, context) =>
      createElement(
        "main",
        {
          className: "zs-story-main",
          "aria-label": context.title,
        },
        createElement(Story),
      ),
  ],
  parameters: {
    themes: {
      disable: true,
    },
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
    layout: "fullscreen",
  },
};

export default preview;
