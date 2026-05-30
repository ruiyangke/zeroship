/*
 * FormSection — a governed settings/form section.
 *
 * The "settings panel" pattern: a labelled section of a settings or form
 * page, with a header (title + description), a body of fields, and an
 * optional footer action row. Composes the field primitives the consumer
 * places (Field/Input), the footer Buttons, the Stack/Cluster layout
 * primitives, and a Separator above the footer.
 *
 *   <FormSection
 *     title="Profile"
 *     description="This information is shown on your public profile."
 *     footer={<><Button variant="ghost">Cancel</Button><Button>Save</Button></>}
 *   >
 *     <Field><Field.Label>Name</Field.Label><Input /></Field>
 *     <Field><Field.Label>Bio</Field.Label><Input /></Field>
 *   </FormSection>
 *
 * Renders as:
 *
 *   <section data-slot="form-section" aria-labelledby={titleId}>
 *     <div data-slot="form-section-header">
 *       <h3 id={titleId} data-slot="form-section-title">Profile</h3>
 *       <p data-slot="form-section-description">…</p>
 *     </div>
 *     <div data-slot="form-section-body">…fields, vertically stacked…</div>
 *     <div data-slot="form-section-footer">
 *       <hr-ish separator />
 *       <div class="cluster" justify=end>…actions…</div>
 *     </div>
 *   </section>
 *
 * ── Dual surface (mirrors Card) ───────────────────────────────────────
 * FormSection has BOTH an ergonomic prop surface and a compound-parts
 * surface, and they compose:
 *
 *   - Ergonomic: pass `title` / `description` / `footer` and let
 *     `children` be the body fields. FormSection builds the Header, wraps
 *     the body in a Stack, and builds the Footer (Separator + right-
 *     aligned Cluster) for you.
 *
 *   - Compound: drop `FormSection.Header` / `FormSection.Body` /
 *     `FormSection.Footer` into `children` for full control of each
 *     region (e.g. a Header.Action button, a custom footer alignment, or
 *     a relevelled Title via `asChild`).
 *
 *   Precedence (documented, no false suppression): the ergonomic
 *   `title`/`description`/`footer` props render their OWN Header/Footer
 *   regions, AND any compound `FormSection.Header`/`.Footer` you place in
 *   `children` ALSO render. They are additive, not exclusive — if you
 *   pass both a `title` prop and a `<FormSection.Header>`, you get two
 *   headers. Pick ONE mode per region. A dev-mode warning fires when
 *   both an ergonomic header prop and a compound Header are detected so
 *   the duplication is diagnosable.
 *
 * ── a11y ──────────────────────────────────────────────────────────────
 * The root is a `<section>` whose accessible name comes from its Title
 * heading via `aria-labelledby`. The Title renders as an `<h3>` by
 * default and is RELEVELABLE via `asChild` so its level matches the
 * surrounding document outline (a section inside an <h2> page wants an
 * <h3>; nested deeper wants <h4>). The id is generated with `useId` and
 * shared between the root and the Title:
 *   - Ergonomic mode: FormSection owns the id and stamps it on both the
 *     root's `aria-labelledby` and the heading it renders.
 *   - Compound mode: the id flows through context. `FormSection.Header`
 *     re-establishes a fresh id (its own `useId`) and publishes it so the
 *     root picks it up — but because the root is rendered BEFORE the
 *     child mounts, compound mode resolves the labelledby relationship by
 *     having BOTH the root and the Title read the SAME context id minted
 *     at the root. The Title stamps that id onto its heading element.
 *
 * Body and Footer are layout-only — plain `<div>`s with NO role. The
 * fields inside carry their own labels/aria (they're the consumer's
 * Field/Input). Footer actions are real Buttons. forced-colors: the
 * footer Separator and header text resolve to system colors (see CSS).
 */
import {
  createContext,
  forwardRef,
  isValidElement,
  useContext,
  useId,
  type ComponentPropsWithoutRef,
  type ReactNode,
  type Ref,
} from "react";
import { Slot } from "../../components/_slot";
import { classnames } from "../../components/_classnames";
import { Separator } from "../../components/Separator";

export type FormSectionOrientation = "stacked" | "aside";

/* ─── shared title-id context ─────────────────────────────────────────
 * The root mints one id (useId) and provides it. The Title (whether
 * rendered by the ergonomic path or via the compound FormSection.Header)
 * stamps that exact id onto its heading element, so the root's
 * `aria-labelledby` always resolves to a present element. */
const FormSectionTitleIdContext = createContext<string | undefined>(undefined);

export interface FormSectionProps
  extends Omit<ComponentPropsWithoutRef<"section">, "title"> {
  /**
   * Section heading. Rendered as a relevelable `<h3>` (see `headingLevel`
   * note on `FormSection.Title`) and used as the section's accessible
   * name via `aria-labelledby`. Ergonomic surface — omit it and provide a
   * `FormSection.Header` in `children` for full control.
   */
  title?: ReactNode;

  /**
   * Muted supporting copy rendered as a `<p>` under the title. Ergonomic
   * surface; the compound equivalent is `FormSection.Description`.
   */
  description?: ReactNode;

  /**
   * Layout orientation.
   * - `stacked` (default): header above body above footer — a single
   *   column. The everyday form section.
   * - `aside`: header in a start column, body in the end column (the
   *   two-column settings-page arrangement); the footer spans the full
   *   width under the body. Collapses to `stacked` below the
   *   `--zs-bp-md` breakpoint.
   */
  orientation?: FormSectionOrientation;

  /**
   * Action row — typically Save/Cancel `Button`s. Rendered under a
   * `Separator` in a right-aligned `Cluster`. Ergonomic surface; the
   * compound equivalent is `FormSection.Footer` (which lets you change
   * the alignment / drop the separator). Omit for a footerless section.
   */
  footer?: ReactNode;

  /**
   * The form body — the consumer's `Field`/`Input` rows — OR a
   * composition of compound parts (`FormSection.Header`,
   * `FormSection.Body`, `FormSection.Footer`). When using the ergonomic
   * `title`/`footer` props, `children` is the body and gets wrapped in a
   * vertical Stack automatically.
   */
  children?: ReactNode;
}

/* ─── root ─────────────────────────────────────────────────────────── */

const FormSectionRoot = forwardRef<HTMLElement, FormSectionProps>(
  function FormSectionRoot(
    {
      title,
      description,
      orientation = "stacked",
      footer,
      className,
      children,
      ...rest
    },
    ref,
  ) {
    // One id, shared by the root's aria-labelledby and the heading the
    // Title renders. Minted here so both ergonomic and compound paths
    // resolve to the same element.
    const titleId = useId();

    // Detect compound regions in children so we can (a) avoid wrapping
    // an explicit FormSection.Body in our own implicit Stack, and (b)
    // dev-warn on additive duplication with the ergonomic props.
    let hasCompoundHeader = false;
    let hasCompoundFooter = false;
    let hasCompoundBody = false;
    const childArray = Array.isArray(children) ? children : [children];
    for (const child of childArray) {
      if (!isValidElement(child)) continue;
      const t = child.type as { displayName?: string } | undefined;
      const name = t?.displayName;
      if (name === "FormSection.Header") hasCompoundHeader = true;
      else if (name === "FormSection.Footer") hasCompoundFooter = true;
      else if (name === "FormSection.Body") hasCompoundBody = true;
    }

    const hasErgonomicHeader = title != null || description != null;
    const hasErgonomicFooter = footer != null;

    if (process.env.NODE_ENV !== "production") {
      if (hasErgonomicHeader && hasCompoundHeader) {
        // eslint-disable-next-line no-console
        console.warn(
          "FormSection: both a `title`/`description` prop AND a " +
            "<FormSection.Header> were provided. They are additive — you " +
            "will get two headers. Use one mode per region.",
        );
      }
      if (hasErgonomicFooter && hasCompoundFooter) {
        // eslint-disable-next-line no-console
        console.warn(
          "FormSection: both a `footer` prop AND a <FormSection.Footer> " +
            "were provided. They are additive — you will get two footers. " +
            "Use one mode per region.",
        );
      }
    }

    // The header region the ergonomic props produce.
    const ergonomicHeader = hasErgonomicHeader ? (
      <FormSectionHeader>
        {title != null ? <FormSectionTitle>{title}</FormSectionTitle> : null}
        {description != null ? (
          <FormSectionDescription>{description}</FormSectionDescription>
        ) : null}
      </FormSectionHeader>
    ) : null;

    // The body region. When the consumer drives the compound surface
    // (any of Header/Body/Footer present), `children` is rendered as-is —
    // they own the structure. Otherwise `children` is the body and we
    // wrap it in the implicit Body region (a vertical Stack).
    const usingCompound =
      hasCompoundHeader || hasCompoundBody || hasCompoundFooter;

    const ergonomicBody =
      !usingCompound && children != null ? (
        <FormSectionBody>{children}</FormSectionBody>
      ) : null;

    // The footer region the ergonomic prop produces.
    const ergonomicFooter = hasErgonomicFooter ? (
      <FormSectionFooter>{footer}</FormSectionFooter>
    ) : null;

    return (
      <FormSectionTitleIdContext.Provider value={titleId}>
        <section
          {...rest}
          ref={ref as Ref<HTMLElement>}
          // The section is named by its Title heading. Both ergonomic
          // and compound Titles stamp THIS id (read from context) onto
          // their heading element, so the relationship always resolves.
          aria-labelledby={titleId}
          data-slot="form-section"
          data-orientation={orientation}
          className={classnames(
            "zs-form-section",
            `zs-form-section--${orientation}`,
            className,
          )}
        >
          {ergonomicHeader}
          {ergonomicBody}
          {usingCompound ? children : null}
          {ergonomicFooter}
        </section>
      </FormSectionTitleIdContext.Provider>
    );
  },
);
FormSectionRoot.displayName = "FormSection";

/* ─── subparts ─────────────────────────────────────────────────────── */

type DivProps = ComponentPropsWithoutRef<"div">;
type HeadingProps = ComponentPropsWithoutRef<"h3">;
type ParagraphProps = ComponentPropsWithoutRef<"p">;

export type FormSectionHeaderProps = DivProps;
export type FormSectionBodyProps = DivProps;
export type FormSectionDescriptionProps = ParagraphProps;

const FormSectionHeader = forwardRef<HTMLDivElement, FormSectionHeaderProps>(
  function FormSectionHeader({ className, ...rest }, ref) {
    // Rest spread BEFORE the internal data-slot so callers cannot
    // overwrite the documented `data-slot="form-section-header"`
    // contract via `{...rest}`.
    return (
      <div
        {...rest}
        ref={ref}
        data-slot="form-section-header"
        className={classnames("zs-form-section__header", className)}
      />
    );
  },
);
FormSectionHeader.displayName = "FormSection.Header";

export interface FormSectionTitleProps extends HeadingProps {
  /**
   * Render-as the single child element so the heading level matches the
   * surrounding document outline (default is `<h3>`; swap to `<h2>`/
   * `<h4>` etc.). The relevelled element still receives the shared title
   * id, so the section's `aria-labelledby` keeps resolving to it.
   */
  asChild?: boolean;
}

const FormSectionTitle = forwardRef<HTMLHeadingElement, FormSectionTitleProps>(
  function FormSectionTitle({ asChild = false, className, children, ...rest }, ref) {
    // Read the shared id minted at the root so the heading element is the
    // exact target of the root's `aria-labelledby`.
    const titleId = useContext(FormSectionTitleIdContext);

    if (asChild) {
      if (!isValidElement(children)) {
        if (process.env.NODE_ENV !== "production") {
          // eslint-disable-next-line no-console
          console.error(
            "FormSection.Title asChild expects a single React element child; received " +
              typeof children +
              "; rendering nothing.",
          );
        }
        return null;
      }
      // Route asChild through Slot for React-19-safe ref / className /
      // style composition. The shared `id` is applied here (after rest,
      // before children) so the relevelled element is the labelledby
      // target. A caller-supplied `id` would override via rest spread —
      // but then the root's aria-labelledby would point at the wrong id,
      // so we lock `id` AFTER rest.
      return (
        <Slot
          {...rest}
          ref={ref as Ref<unknown>}
          id={titleId}
          data-slot="form-section-title"
          className={classnames("zs-form-section__title", className)}
        >
          {children}
        </Slot>
      );
    }

    // Rest spread BEFORE the internal contract attrs. `id` is locked
    // AFTER rest so the heading is always the aria-labelledby target.
    return (
      <h3
        {...rest}
        ref={ref}
        id={titleId}
        data-slot="form-section-title"
        className={classnames("zs-form-section__title", className)}
      >
        {children}
      </h3>
    );
  },
);
FormSectionTitle.displayName = "FormSection.Title";

const FormSectionDescription = forwardRef<
  HTMLParagraphElement,
  FormSectionDescriptionProps
>(function FormSectionDescription({ className, ...rest }, ref) {
  // Rest spread BEFORE the internal data-slot contract.
  return (
    <p
      {...rest}
      ref={ref}
      data-slot="form-section-description"
      className={classnames("zs-form-section__description", className)}
    />
  );
});
FormSectionDescription.displayName = "FormSection.Description";

const FormSectionBody = forwardRef<HTMLDivElement, FormSectionBodyProps>(
  function FormSectionBody({ className, ...rest }, ref) {
    // Layout-only: a vertical stack of the consumer's fields. No role —
    // the fields inside carry their own labels/aria. The vertical rhythm
    // comes from the CSS (`gap`), not a Stack component, so the body is a
    // single plain element the consumer can target via data-slot.
    return (
      <div
        {...rest}
        ref={ref}
        data-slot="form-section-body"
        className={classnames("zs-form-section__body", className)}
      />
    );
  },
);
FormSectionBody.displayName = "FormSection.Body";

export type FormSectionFooterAlign = "start" | "between" | "end";

export interface FormSectionFooterProps extends DivProps {
  /**
   * Justify-content of the action Cluster. Default `end` (actions sit on
   * the trailing inline edge — the conventional Save/Cancel placement).
   */
  align?: FormSectionFooterAlign;
  /**
   * Render the hairline `Separator` above the action row. Default `true`
   * — the footer reads as a distinct affordance band. Set `false` for a
   * flush footer.
   */
  separator?: boolean;
}

const FormSectionFooter = forwardRef<HTMLDivElement, FormSectionFooterProps>(
  function FormSectionFooter(
    { align = "end", separator = true, className, children, ...rest },
    ref,
  ) {
    // Rest spread BEFORE the internal contract attrs (data-slot /
    // data-align) so callers cannot desync them via raw spread — the
    // documented surface is the `align` prop.
    return (
      <div
        {...rest}
        ref={ref}
        data-slot="form-section-footer"
        data-align={align}
        className={classnames("zs-form-section__footer", className)}
      >
        {separator ? (
          <Separator
            className="zs-form-section__footer-divider"
            data-slot="form-section-footer-divider"
          />
        ) : null}
        <div
          data-slot="form-section-actions"
          className="zs-form-section__actions"
        >
          {children}
        </div>
      </div>
    );
  },
);
FormSectionFooter.displayName = "FormSection.Footer";

/* ─── public namespace ─────────────────────────────────────────────── */

type FormSectionComponent = typeof FormSectionRoot & {
  Header: typeof FormSectionHeader;
  Title: typeof FormSectionTitle;
  Description: typeof FormSectionDescription;
  Body: typeof FormSectionBody;
  Footer: typeof FormSectionFooter;
};

export const FormSection = FormSectionRoot as FormSectionComponent;
FormSection.Header = FormSectionHeader;
FormSection.Title = FormSectionTitle;
FormSection.Description = FormSectionDescription;
FormSection.Body = FormSectionBody;
FormSection.Footer = FormSectionFooter;
