/*
 * SectionFrame + SectionHeader — governed page-composition primitives.
 *
 * These are deliberately smaller than a finished marketing section such as
 * Hero or FeatureGrid. They capture the page geometry we kept hand-rolling in
 * the Apple clone: a full-bleed band, one governed inner width, exact vertical
 * rhythm, and a reusable header row that can split title/actions like premium
 * product pages do.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ElementType,
  type ReactNode,
} from "react";
import { classnames } from "../../components/_classnames";
import { Container, type ContainerSize } from "../../layouts/Container";
import { type Pad } from "../../layouts/_layout-primitives";
import type { SectionTone } from "../_tone";

export type SectionFrameSpacing = "none" | "compact" | "regular" | "spacious";
export type SectionHeaderAlign = "start" | "center";
export type SectionHeaderLayout = "stack" | "split";
export type SectionHeaderTitleSize = "display" | "large" | "title";

export interface SectionFrameProps
  extends ComponentPropsWithoutRef<"section"> {
  /** Full-bleed band tone shared with the section system. */
  tone?: SectionTone;
  /** Vertical band rhythm. Default `regular`. */
  spacing?: SectionFrameSpacing;
  /** Governed inner content width. Default `wide`. */
  size?: ContainerSize;
  /** Inner inline padding, from the spacing scale. Default `6`. */
  padX?: Pad;
  /** Optional content before the main body, typically `<SectionHeader />`. */
  header?: ReactNode;
  /** Root `data-slot` value. Defaults to `"section-frame"`. */
  "data-slot"?: string;
}

export interface SectionHeaderProps
  extends Omit<ComponentPropsWithoutRef<"div">, "title"> {
  eyebrow?: ReactNode;
  title?: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  /** Heading element used for the title. Default `h2`. */
  titleAs?: Extract<ElementType, "h1" | "h2" | "h3" | "h4">;
  /** Header alignment. Default `start`. */
  align?: SectionHeaderAlign;
  /** `split` places actions at the inline-end on wide viewports. */
  layout?: SectionHeaderLayout;
  /** Display scale for the title. Default `display`. */
  titleSize?: SectionHeaderTitleSize;
  /** Root `data-slot` value. Defaults to `"section-header"`. */
  "data-slot"?: string;
}

export const SectionHeader = forwardRef<HTMLDivElement, SectionHeaderProps>(
  function SectionHeader(
    {
      eyebrow,
      title,
      description,
      actions,
      titleAs: Title = "h2",
      align = "start",
      layout = "split",
      titleSize = "display",
      className,
      children,
      "data-slot": dataSlot = "section-header",
      ...rest
    },
    ref,
  ) {
    return (
      <div
        {...rest}
        ref={ref}
        data-slot={dataSlot}
        data-align={align}
        data-layout={layout}
        data-title-size={titleSize}
        className={classnames("zs-section-header", className)}
      >
        <div className="zs-section-header__text" data-slot="section-header-text">
          {eyebrow != null ? (
            <div
              className="zs-section-header__eyebrow zs-section-eyebrow"
              data-slot="section-header-eyebrow"
            >
              {eyebrow}
            </div>
          ) : null}
          {title != null ? (
            <Title
              className="zs-section-header__title"
              data-slot="section-header-title"
            >
              {title}
            </Title>
          ) : null}
          {description != null ? (
            <p
              className="zs-section-header__description"
              data-slot="section-header-description"
            >
              {description}
            </p>
          ) : null}
          {children}
        </div>
        {actions != null ? (
          <div
            className="zs-section-header__actions"
            data-slot="section-header-actions"
          >
            {actions}
          </div>
        ) : null}
      </div>
    );
  },
);
SectionHeader.displayName = "SectionHeader";

export const SectionFrame = forwardRef<HTMLElement, SectionFrameProps>(
  function SectionFrame(
    {
      tone = "default",
      spacing = "regular",
      size = "wide",
      padX = 6,
      header,
      className,
      children,
      "data-slot": dataSlot = "section-frame",
      ...rest
    },
    ref,
  ) {
    return (
      <section
        {...rest}
        ref={ref}
        data-slot={dataSlot}
        data-section-band=""
        data-tone={tone}
        data-spacing={spacing}
        className={classnames("zs-section-frame", className)}
      >
        <Container
          size={size}
          padX={padX}
          data-slot="section-frame-inner"
          className="zs-section-frame__inner"
        >
          {header != null ? header : null}
          {children}
        </Container>
      </section>
    );
  },
);
SectionFrame.displayName = "SectionFrame";
