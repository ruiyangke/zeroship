import type { Preview } from "@storybook/react";
import { createElement } from "react";
import { withThemeByDataAttribute } from "@storybook/addon-themes";
import "../src/styles.css";
import "../src/stories/story.css";

const preview: Preview = {
  decorators: [
    withThemeByDataAttribute({
      themes: { Crystal: "crystal" },
      defaultTheme: "Crystal",
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
    /* addon-coverage instruments component sources via vite-plugin-istanbul
     * at preview build time. Stories and tests are excluded so coverage
     * % reflects the actual surface under test, not story scaffolding. */
    coverage: {
      include: ["src/**/*.{ts,tsx}"],
      exclude: [
        "src/**/*.stories.tsx",
        "src/**/*.test.ts",
        "src/stories/**",
      ],
    },
  },
};

export default preview;
