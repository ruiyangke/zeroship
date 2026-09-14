import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";

/**
 * Theme names registered with the design system.
 *
 * Themes vary only palette/accent/material defaults — the design
 * language itself (type scale, spacing, radii, motion, focus, hit
 * targets) is theme-invariant and lives in the foundation token block
 * in styles.css.
 *
 * Currently registered themes: `crystal-light`, `crystal-dark`,
 * `studio-light`, and `ghibli-light`.
 */
export const themes = [
  "crystal-light",
  "crystal-dark",
  "studio-light",
  "ghibli-light",
] as const;
export type ThemeName = (typeof themes)[number] | (string & {});

export const DEFAULT_THEME: ThemeName = "crystal-light";
export const THEME_STORAGE_KEY = "zeroship-ui-theme";

export const themeLabels: Record<string, string> = {
  "crystal-light": "Crystal Light",
  "crystal-dark": "Crystal Dark",
  "studio-light": "Studio Light",
  "ghibli-light": "Ghibli Light",
};

export function isThemeName(value: string | null | undefined): value is ThemeName {
  return themes.includes(value as (typeof themes)[number]);
}

interface ThemeContextValue {
  theme: ThemeName;
  setTheme: (theme: ThemeName) => void;
  themes: typeof themes;
}

export interface ThemeProviderProps {
  children: ReactNode;
  defaultTheme?: ThemeName;
  theme?: ThemeName;
  storageKey?: string;
  persist?: boolean;
  applyToDocument?: boolean;
}

const ThemeContext = createContext<ThemeContextValue | null>(null);

function readStoredTheme(storageKey: string, fallback: ThemeName): ThemeName {
  if (typeof window === "undefined") return fallback;
  const stored = window.localStorage.getItem(storageKey);
  return isThemeName(stored) ? stored : fallback;
}

export function ThemeProvider({
  children,
  defaultTheme = DEFAULT_THEME,
  theme: controlledTheme,
  storageKey = THEME_STORAGE_KEY,
  persist = true,
  applyToDocument = true,
}: ThemeProviderProps) {
  const [uncontrolledTheme, setUncontrolledTheme] = useState<ThemeName>(() =>
    readStoredTheme(storageKey, defaultTheme),
  );

  const theme = controlledTheme ?? uncontrolledTheme;

  const setTheme = useCallback(
    (next: ThemeName) => {
      if (controlledTheme === undefined) setUncontrolledTheme(next);
      if (persist && typeof window !== "undefined") {
        window.localStorage.setItem(storageKey, next);
      }
    },
    [controlledTheme, persist, storageKey],
  );

  useEffect(() => {
    if (!applyToDocument || typeof document === "undefined") return;
    document.documentElement.dataset.theme = theme;
  }, [applyToDocument, theme]);

  const value = useMemo<ThemeContextValue>(
    () => ({ theme, setTheme, themes }),
    [theme, setTheme],
  );

  return (
    <ThemeContext.Provider value={value}>
      <div className="zs-theme-root" data-theme={theme}>
        {children}
      </div>
    </ThemeContext.Provider>
  );
}

export function useTheme(): ThemeContextValue {
  const ctx = useContext(ThemeContext);
  if (!ctx) {
    throw new Error("useTheme must be used within ThemeProvider");
  }
  return ctx;
}
