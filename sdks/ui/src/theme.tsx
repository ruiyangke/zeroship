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
 * The HIG-anchored rebuild will populate this with `hig-light` and `hig-dark`
 * as the two first-class themes. Until the foundation tokens land, the list
 * is empty and ThemeProvider becomes a passthrough that still establishes a
 * `data-theme` root for future styling.
 */
export const themes = [] as const;
export type ThemeName = (typeof themes)[number] | (string & {});

export const DEFAULT_THEME: ThemeName = "hig-light";
export const THEME_STORAGE_KEY = "zeroship-ui-theme";

export const themeLabels: Record<string, string> = {};

export function isThemeName(value: string | null | undefined): value is ThemeName {
  return typeof value === "string" && value.length > 0;
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
