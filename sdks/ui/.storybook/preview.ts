import type { Preview } from "@storybook/react";
import { createElement } from "react";
import { withThemeByDataAttribute } from "@storybook/addon-themes";
import "../src/styles.css";

/*
 * The themes map is intentionally empty during the Apple-HIG rebuild.
 * It is repopulated with `hig-light` / `hig-dark` (and any future palette
 * variants) once the foundation tokens land. The decorator stays installed
 * so the toolbar shape is preserved.
 */
const preview: Preview = {
  decorators: [
    withThemeByDataAttribute({
      themes: {},
      defaultTheme: "hig-light",
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
