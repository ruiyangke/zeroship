/**
 * @zeroship/ui — public surface.
 *
 * Mid-rebuild. The exports below are intentionally narrow: theme primitives
 * plus the styled components that have landed so far, plus temporary
 * native-HTML placeholders for the names other packages still import.
 *
 * As styled components land they replace the corresponding placeholder
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

// Real, styled components.
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
  Card,
  type CardProps,
  type CardVariant,
  type CardSize,
  type CardMediaSide,
  type CardMediaProps,
  type CardFooterAlign,
  type CardFooterDivider,
  type CardFooterProps,
  type CardHeaderProps,
  type CardTitleProps,
  type CardDescriptionProps,
  type CardActionProps,
  type CardContentProps,
  Dialog,
  createDialogHandle,
  type DialogProps,
  type DialogSize,
  type DialogPlacement,
  type DialogBackdropTint,
  type DialogBackdropProps,
  type DialogPopupProps,
  type DialogHeaderProps,
  type DialogTitleProps,
  type DialogDescriptionProps,
  type DialogBodyProps,
  type DialogFooterProps,
  type DialogCloseProps,
  type DialogTriggerProps,
  type DialogPortalProps,
  type DialogViewportProps,
  AlertDialog,
  type AlertDialogProps,
  type AlertDialogSize,
  type AlertDialogActionTone,
  type AlertDialogTriggerProps,
  type AlertDialogPortalProps,
  type AlertDialogBackdropProps,
  type AlertDialogPopupProps,
  type AlertDialogHeaderProps,
  type AlertDialogTitleProps,
  type AlertDialogDescriptionProps,
  type AlertDialogBodyProps,
  type AlertDialogFooterProps,
  type AlertDialogCancelProps,
  type AlertDialogActionProps,
} from "./components";

// Temporary placeholders — see ./placeholders.tsx for migration tracking.
export {
  Badge,
  type BadgeProps,
} from "./placeholders";
