// Re-exports include the full set of subpart prop types
// (Title/Description/Body/Footer) — review-fix item 17 — plus
// Viewport and `createDialogHandle` from review-fix item 11.
export { Dialog, createDialogHandle } from "./Dialog";
export type {
  DialogProps,
  DialogSize,
  DialogPlacement,
  DialogBackdropTint,
  DialogBackdropProps,
  DialogPopupProps,
  DialogHeaderProps,
  DialogTitleProps,
  DialogDescriptionProps,
  DialogBodyProps,
  DialogFooterProps,
  DialogCloseProps,
  DialogTriggerProps,
  DialogPortalProps,
  DialogViewportProps,
} from "./Dialog";
