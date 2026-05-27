/**
 * @zeroship/ui — public surface.
 *
 * Mid-rebuild against Apple Human Interface Guidelines. The exports below
 * are intentionally narrow: theme primitives plus the HIG-styled
 * components that have landed so far, plus temporary native-HTML
 * placeholders for the names other packages still import.
 *
 * As HIG-styled components land they replace the corresponding placeholder
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

// Real, HIG-styled components.
export {
  Button,
  type ButtonProps,
  type ButtonIntent,
  type ButtonSize,
  type ButtonVariant,
  Field,
  type FieldProps,
  type FieldOrientation,
  type FieldSize,
  type FieldRequiredProps,
  Input,
  type InputProps,
  type InputSize,
  type InputVariant,
} from "./components";

// Temporary placeholders — see ./placeholders.tsx for migration tracking.
export {
  Badge,
  Card,
  Dialog,
  type BadgeProps,
  type CardProps,
  type DialogProps,
} from "./placeholders";
