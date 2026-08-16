import { Drawer as BaseDrawer } from "@base-ui/react/drawer";
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type Ref,
} from "react";

export type DrawerSide = "start" | "end" | "top" | "bottom";
export type DrawerSize = "sm" | "md" | "lg" | "full";

export type DrawerProps = BaseDrawer.Root.Props;

const DrawerRoot = ({ swipeDirection = "right", ...props }: DrawerProps) => {
  return <BaseDrawer.Root {...props} swipeDirection={swipeDirection} />;
};

const DrawerPortal = (props: BaseDrawer.Portal.Props) => {
  return <BaseDrawer.Portal {...props} />;
};

const DrawerBackdrop = forwardRef<
  HTMLDivElement,
  BaseDrawer.Backdrop.Props
>(function DrawerBackdrop(props, ref) {
  return (
    <BaseDrawer.Backdrop
      {...props}
      ref={ref}
      data-slot="drawer-backdrop"
    />
  );
});

export interface DrawerContentProps extends BaseDrawer.Popup.Props {
  side?: DrawerSide;
  size?: DrawerSize;
}

const DrawerContent = forwardRef<HTMLDivElement, DrawerContentProps>(
  function DrawerContent(
    { side = "end", size = "md", children, ...props },
    ref,
  ) {
    return (
      <BaseDrawer.Viewport>
        <BaseDrawer.Popup
          {...props}
          ref={ref}
          render={<BaseDrawer.Content />}
          data-base-ui-swipe-ignore=""
          data-slot="drawer-content"
          data-side={side}
          data-size={size}
        >
          {children}
        </BaseDrawer.Popup>
      </BaseDrawer.Viewport>
    );
  },
);

function CloseIcon() {
  return (
    <svg
      aria-hidden="true"
      data-size="sm"
      data-slot="icon"
      fill="none"
      focusable="false"
      stroke="currentColor"
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth="2"
      viewBox="0 0 24 24"
    >
      <path d="M18 6 6 18" />
      <path d="m6 6 12 12" />
    </svg>
  );
}

export interface DrawerHeaderProps extends ComponentPropsWithoutRef<"div"> {
  showClose?: boolean;
  closeLabel?: string;
}

const DrawerHeader = forwardRef<HTMLDivElement, DrawerHeaderProps>(
  function DrawerHeader(
    { showClose = true, closeLabel = "Close", children, ...props },
    ref,
  ) {
    return (
      <div {...props} ref={ref} data-slot="drawer-header">
        <div data-slot="drawer-header-content">{children}</div>
        {showClose ? (
          <BaseDrawer.Close
            aria-label={closeLabel}
            data-slot="drawer-header-close"
          >
            <CloseIcon />
          </BaseDrawer.Close>
        ) : null}
      </div>
    );
  },
);

const DrawerTitle = (props: BaseDrawer.Title.Props) => {
  return <BaseDrawer.Title {...props} data-slot="drawer-title" />;
};

const DrawerDescription = (props: BaseDrawer.Description.Props) => {
  return (
    <BaseDrawer.Description {...props} data-slot="drawer-description" />
  );
};

const DrawerBody = forwardRef<
  HTMLDivElement,
  ComponentPropsWithoutRef<"div">
>(function DrawerBody(props, ref) {
  return <div {...props} ref={ref} data-slot="drawer-body" />;
});

const DrawerFooter = forwardRef<
  HTMLDivElement,
  ComponentPropsWithoutRef<"div">
>(function DrawerFooter(props, ref) {
  return <div {...props} ref={ref} data-slot="drawer-footer" />;
});

DrawerBackdrop.displayName = "Drawer.Backdrop";
DrawerContent.displayName = "Drawer.Content";
DrawerHeader.displayName = "Drawer.Header";
DrawerBody.displayName = "Drawer.Body";
DrawerFooter.displayName = "Drawer.Footer";

export const Drawer = Object.assign(DrawerRoot, {
  Portal: DrawerPortal,
  Backdrop: DrawerBackdrop,
  Content: DrawerContent,
  Header: DrawerHeader,
  Title: DrawerTitle,
  Description: DrawerDescription,
  Body: DrawerBody,
  Footer: DrawerFooter,
});
