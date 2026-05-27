import type { StorybookConfig } from "@storybook/react-vite";

const config: StorybookConfig = {
  stories: ["../src/**/*.mdx", "../src/**/*.stories.@(ts|tsx)"],
  addons: [
    "@storybook/addon-docs",
    "@storybook/addon-a11y",
    "@storybook/addon-themes",
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
};

export default config;
