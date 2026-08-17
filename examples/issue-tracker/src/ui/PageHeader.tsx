import {
  forwardRef,
  type ComponentPropsWithoutRef,
} from "react";

export type PageHeaderProps = ComponentPropsWithoutRef<"div">;
export type PageHeaderTitleProps = ComponentPropsWithoutRef<"h1">;
export type PageHeaderDescriptionProps = ComponentPropsWithoutRef<"p">;

const ROOT_CLASSES =
  "grid min-w-0 grid-cols-[minmax(0,1fr)_auto] items-start gap-x-4 gap-y-1 border-b border-line-subtle pb-2";
const TITLE_CLASSES =
  "col-start-1 m-0 min-w-0 text-xl font-semibold leading-tight text-ink";
const DESCRIPTION_CLASSES =
  "col-start-1 m-0 min-w-0 text-base leading-snug text-ink-secondary";

const PageHeaderRoot = forwardRef<HTMLDivElement, PageHeaderProps>(
  function PageHeaderRoot({ className, children, ...props }, ref) {
    return (
      <div
        {...props}
        ref={ref}
        data-slot="page-header"
        className={`${ROOT_CLASSES}${className ? ` ${className}` : ""}`}
      >
        {children}
      </div>
    );
  },
);
PageHeaderRoot.displayName = "PageHeader";

const PageHeaderTitle = forwardRef<HTMLHeadingElement, PageHeaderTitleProps>(
  function PageHeaderTitle({ className, children, ...props }, ref) {
    return (
      <h1
        {...props}
        ref={ref}
        data-slot="page-header-title"
        className={`${TITLE_CLASSES}${className ? ` ${className}` : ""}`}
      >
        {children}
      </h1>
    );
  },
);
PageHeaderTitle.displayName = "PageHeader.Title";

const PageHeaderDescription = forwardRef<
  HTMLParagraphElement,
  PageHeaderDescriptionProps
>(function PageHeaderDescription({ className, children, ...props }, ref) {
  return (
    <p
      {...props}
      ref={ref}
      data-slot="page-header-description"
      className={`${DESCRIPTION_CLASSES}${className ? ` ${className}` : ""}`}
    >
      {children}
    </p>
  );
});
PageHeaderDescription.displayName = "PageHeader.Description";

type PageHeaderComponent = typeof PageHeaderRoot & {
  Title: typeof PageHeaderTitle;
  Description: typeof PageHeaderDescription;
};

export const PageHeader = PageHeaderRoot as PageHeaderComponent;
PageHeader.Title = PageHeaderTitle;
PageHeader.Description = PageHeaderDescription;
