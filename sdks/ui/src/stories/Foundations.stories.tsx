import { useEffect, useMemo, useState } from "react";
import type { Meta, StoryObj } from "@storybook/react";
import {
  colorTokenNames,
  DEFAULT_THEME,
  fontTokenNames,
  radiusTokenNames,
  shadowTokenNames,
  spaceTokenNames,
  textTokenNames,
  themeLabels,
  themes,
  type ThemeName,
} from "../index";

function useActiveTheme(): ThemeName {
  const read = () => {
    const raw = document.documentElement.dataset.theme;
    return themes.includes(raw as ThemeName) ? (raw as ThemeName) : DEFAULT_THEME;
  };
  const [theme, setTheme] = useState<ThemeName>(() =>
    typeof document === "undefined" ? DEFAULT_THEME : read(),
  );

  useEffect(() => {
    const observer = new MutationObserver(() => setTheme(read()));
    observer.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-theme"],
    });
    return () => observer.disconnect();
  }, []);

  return theme;
}

function cssValue(name: string) {
  if (typeof document === "undefined") return "";
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

function TokenValue({ name }: { name: string }) {
  const theme = useActiveTheme();
  const value = useMemo(() => cssValue(name), [name, theme]);
  return <span className="zs-token-swatch__value">{value}</span>;
}

function ColorSwatches() {
  useActiveTheme();
  return (
    <div className="zs-token-grid">
      {colorTokenNames.map((token) => (
        <div className="zs-token-swatch" key={token}>
          <div
            className="zs-token-swatch__sample"
            style={{ background: `var(${token})` }}
          />
          <span className="zs-token-swatch__name">{token}</span>
          <TokenValue name={token} />
        </div>
      ))}
    </div>
  );
}

function TypeScale() {
  useActiveTheme();
  return (
    <div className="zs-story-stack">
      {textTokenNames.map((token) => (
        <div className="zs-type-sample" key={token}>
          <div className="zs-type-sample__name">{token}</div>
          <div style={{ fontSize: `var(${token})`, lineHeight: "var(--zs-line-normal)" }}>
            The platform earns only when creators earn.
          </div>
          <TokenValue name={token} />
        </div>
      ))}
      {fontTokenNames.map((token) => (
        <div className="zs-type-sample" key={token}>
          <div className="zs-type-sample__name">{token}</div>
          <div style={{ fontFamily: `var(${token})`, fontSize: "var(--zs-text-xl)" }}>
            Anyone can create, launch, and monetize software.
          </div>
          <TokenValue name={token} />
        </div>
      ))}
    </div>
  );
}

function SpaceRadiusShadow() {
  useActiveTheme();
  return (
    <div className="zs-story-grid">
      <section className="zs-story-stack">
        <h2 className="zs-story-title">Space</h2>
        {spaceTokenNames.map((token) => (
          <div className="zs-token-swatch" key={token}>
            <span className="zs-token-swatch__name">{token}</span>
            <div className="zs-size-bar" style={{ width: `var(${token})` }} />
            <TokenValue name={token} />
          </div>
        ))}
      </section>
      <section className="zs-story-stack">
        <h2 className="zs-story-title">Radius</h2>
        {radiusTokenNames.map((token) => (
          <div className="zs-token-swatch" key={token}>
            <span className="zs-token-swatch__name">{token}</span>
            <div className="zs-radius-sample" style={{ borderRadius: `var(${token})` }} />
            <TokenValue name={token} />
          </div>
        ))}
      </section>
      <section className="zs-story-stack">
        <h2 className="zs-story-title">Shadow</h2>
        {shadowTokenNames.map((token) => (
          <div className="zs-token-swatch" key={token}>
            <span className="zs-token-swatch__name">{token}</span>
            <div className="zs-shadow-sample" style={{ boxShadow: `var(${token})` }} />
            <TokenValue name={token} />
          </div>
        ))}
      </section>
    </div>
  );
}

function Foundations() {
  const theme = useActiveTheme();
  return (
    <div className="zs-story-shell zs-story-stack">
      <div className="zs-story-stack">
        <h1 className="zs-story-title">Foundations: {themeLabels[theme]}</h1>
        <p className="zs-story-subtle">
          These swatches read the active Storybook toolbar theme from
          data-theme and render the same semantic contract under each theme.
        </p>
      </div>
      <ColorSwatches />
      <TypeScale />
      <SpaceRadiusShadow />
    </div>
  );
}

const meta = {
  title: "Foundations/Tokens",
  component: Foundations,
  tags: ["autodocs"],
} satisfies Meta<typeof Foundations>;

export default meta;
type Story = StoryObj<typeof meta>;

export const ActiveTheme: Story = {};
