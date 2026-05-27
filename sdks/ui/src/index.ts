/**
 * @zeroship/ui — public surface.
 *
 * Mid-rebuild against Apple Human Interface Guidelines. The exports below
 * are intentionally narrow: theme primitives plus temporary native-HTML
 * placeholders for the component names other packages still import. As
 * HIG-styled components land they replace the corresponding placeholder
 * exports in this file.
 */
import "./styles.css";

export {
  DEFAULT_THEME,
  THEME_STORAGE_KEY,
  ThemeProvider,
  isThemeName,
  themeLabels,
  themes,
  useTheme,
  type ThemeName,
  type ThemeProviderProps,
} from "./theme";

// Real, HIG-styled components. Currently: Button.
export {
  Button,
  type ButtonProps,
  type ButtonRole,
  type ButtonSize,
  type ButtonVariant,
} from "./components";

// Temporary placeholders — see ./placeholders.tsx for migration tracking.
export {
  Badge,
  Card,
  Dialog,
  Input,
  type BadgeProps,
  type CardProps,
  type DialogProps,
  type InputProps,
} from "./placeholders";
