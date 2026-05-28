import type { StorybookConfig } from "@storybook/react-vite";
import { createStorybookMcpMiddleware } from "./mcp-server";

const config: StorybookConfig = {
  stories: ["../src/**/*.mdx", "../src/**/*.stories.@(ts|tsx)"],
  addons: [
    "@storybook/addon-docs",
    "@storybook/addon-a11y",
    "@storybook/addon-themes",
    "@storybook/addon-coverage",
  ],
  framework: {
    name: "@storybook/react-vite",
    options: {},
  },
  docs: {
    /* Generate a Docs page for every component automatically. The prop
     * table is built from react-docgen-typescript + JSDoc. */
    autodocs: true,
  },
  typescript: {
    /* react-docgen-typescript reads our prop interfaces with full
     * fidelity — literal unions ('outline' | 'filled' | 'plain'),
     * JSDoc on each prop, and default values from forwardRef body
     * destructuring. The cheaper `react-docgen` default loses most of
     * that. */
    reactDocgen: "react-docgen-typescript",
    reactDocgenTypescriptOptions: {
      shouldExtractLiteralValuesFromEnum: true,
      shouldRemoveUndefinedFromOptional: true,
      /* Skip inherited DOM props (HTMLDivElement, HTMLInputElement,
       * HTMLButtonElement, etc.) so the prop table shows only what
       * each component itself adds. Without this filter, Card props
       * would include ~250 inherited <div> attributes — useless
       * noise. */
      propFilter: (prop) =>
        !prop.parent ||
        (!/node_modules/.test(prop.parent.fileName) &&
          !/\bReact\./.test(prop.parent.name)),
    },
  },
  /* Mount the MCP HTTP handler at `/mcp` on the Storybook dev server.
   * AI agents discover components, story IDs, and prop tables via the
   * standard Storybook MCP tools. The Test Runner exposes its own
   * tool surface to the same server. */
  async viteFinal(viteConfig) {
    const mcpMiddleware = await createStorybookMcpMiddleware();
    return {
      ...viteConfig,
      plugins: [
        ...(viteConfig.plugins ?? []),
        {
          name: "zeroship-ui-mcp",
          configureServer(server) {
            server.middlewares.use(mcpMiddleware);
          },
          configurePreviewServer(server) {
            server.middlewares.use(mcpMiddleware);
          },
        },
      ],
    };
  },
};

export default config;
