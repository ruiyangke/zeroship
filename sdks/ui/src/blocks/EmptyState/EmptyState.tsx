/*
 * EmptyState — the "nothing here yet" panel.
 *
 * The canonical empty-collection / zero-results / first-run surface:
 * a centered column of an optional decorative icon, a heading, a muted
 * description, and an optional action row (typically a <Button>).
 *
 * Like Card, EmptyState exposes a DUAL surface:
 *
 *   1. Ergonomic props — the 80% case in one element:
 *        <EmptyState
 *          icon={<InboxIcon />}
 *          title="No messages"
 *          description="When someone writes to you it shows up here."
 *          action={<Button>Compose</Button>}
 *        />
 *
 *   2. Compound parts — full control over composition / ordering:
 *        <EmptyState>
 *          <EmptyState.Icon><InboxIcon /></EmptyState.Icon>
 *          <EmptyState.Title>No messages</EmptyState.Title>
 *          <EmptyState.Description>…</EmptyState.Description>
 *          <EmptyState.Actions><Button>Compose</Button></EmptyState.Actions>
 *        </EmptyState>
 *
 * Use ONE mode. The two surfaces are not mutually exclusive at runtime —
 * they compose: ergonomic-prop content renders FIRST, then any compound
 * children render after it inside the same centered column. There is no
 * suppression, so supplying both a `title` prop AND an
 * `<EmptyState.Title>` child renders TWO headings. Pick one mode per
 * panel.
 *
 * Layout dogfoods the Wave 1 primitives: a `Center` provides the
 * both-axes centering and a column `Stack` governs the gap between
 * icon / title / description / actions. We never re-roll flexbox here.
 *
 * Aria: an EmptyState is NOT an error — it carries no `role="alert"`
 * and asserts nothing to assistive tech on mount. The title renders as
 * a real heading (`<h2>` by default; `asChild` on the Title relevels it
 * to match the surrounding document outline). The icon wrapper is
 * `aria-hidden` because it is decorative — the heading carries the
 * meaning.
 *
 * This block has no root `asChild` — its value is the composed column
 * (icon + title + description + actions); routing the root through a
 * Slot would render only the consumer's child and discard that column.
 * Wrap the block in your own element if you need a custom/semantic root
 * (e.g. a `<section aria-labelledby>`).
 *
 * `data-slot="empty-state"` (and `empty-state-<part>` on each subpart)
 * mirrors the Card data-slot vocabulary so consumers can target parts
 * in CSS without leaning on the internal BEM class names.
 */
import {
  forwardRef,
  isValidElement,
  type ComponentPropsWithoutRef,
  type ReactNode,
  type Ref,
} from "react";
import { Slot } from "../../components/_slot";
import { classnames } from "../../components/_classnames";
import { Center } from "../../layouts/Center";
import { Stack } from "../../layouts/Stack";

export interface EmptyStateProps
  extends Omit<ComponentPropsWithoutRef<"div">, "title"> {
  /**
   * Decorative leading glyph (an icon element). Wrapped in an
   * `aria-hidden` container — the title carries the accessible meaning,
   * so the icon never doubles up the announcement.
   */
  icon?: ReactNode;

  /**
   * Headline. Rendered as the panel's heading (an `<h2>` by default).
   * This is the ergonomic mode. To relevel the heading or otherwise
   * control composition, use the compound `<EmptyState.Title>` part
   * instead (e.g. `<EmptyState.Title asChild>`). Use ONE mode: there is
   * no suppression, so passing both this prop AND an `<EmptyState.Title>`
   * child renders two headings (prop content first, then children).
   */
  title?: ReactNode;

  /** Muted supporting copy beneath the title. */
  description?: ReactNode;

  /**
   * Action row — typically a `<Button>` (or a small cluster of them).
   * Rendered last in the column.
   */
  action?: ReactNode;

  /** Panel contents — compound parts and/or arbitrary children. */
  children?: ReactNode;
}

/* ─── EmptyState root ────────────────────────────────────────────────── */

const EmptyStateRoot = forwardRef<HTMLDivElement, EmptyStateProps>(
  function EmptyStateRoot(
    { icon, title, description, action, className, children, ...rest },
    ref,
  ) {
    const composedClassName = classnames("zs-empty-state", className);

    // The centered column. `Center` handles both-axes centering; the
    // inner `Stack` governs the icon→title→description→actions gap. The
    // ergonomic-prop content renders first, then any compound children.
    const column = (
      <Center asChild data-slot="empty-state-column">
        <Stack className="zs-empty-state__column" align="center" gap={3}>
          {icon != null ? <EmptyStateIcon>{icon}</EmptyStateIcon> : null}
          {title != null ? <EmptyStateTitle>{title}</EmptyStateTitle> : null}
          {description != null ? (
            <EmptyStateDescription>{description}</EmptyStateDescription>
          ) : null}
          {action != null ? <EmptyStateActions>{action}</EmptyStateActions> : null}
          {children}
        </Stack>
      </Center>
    );

    return (
      <div
        {...rest}
        ref={ref}
        data-slot="empty-state"
        className={composedClassName}
      >
        {column}
      </div>
    );
  },
);
EmptyStateRoot.displayName = "EmptyState";

/* ─── Subparts ───────────────────────────────────────────────────────── */

type DivProps = ComponentPropsWithoutRef<"div">;
type ParagraphProps = ComponentPropsWithoutRef<"p">;
type HeadingProps = ComponentPropsWithoutRef<"h2">;

export type EmptyStateIconProps = DivProps;
export type EmptyStateDescriptionProps = ParagraphProps;
export type EmptyStateActionsProps = DivProps;

const EmptyStateIcon = forwardRef<HTMLDivElement, EmptyStateIconProps>(
  function EmptyStateIcon({ className, ...rest }, ref) {
    // Decorative — aria-hidden BEFORE rest so a consumer who genuinely
    // wants a meaningful icon can opt back in with aria-hidden={false}.
    return (
      <div
        aria-hidden="true"
        {...rest}
        ref={ref}
        data-slot="empty-state-icon"
        className={classnames("zs-empty-state__icon", className)}
      />
    );
  },
);
EmptyStateIcon.displayName = "EmptyState.Icon";

export interface EmptyStateTitleProps extends HeadingProps {
  /** Render-as the single child element — e.g. to relevel the heading
   *  (`<EmptyState.Title asChild><h3>…</h3></EmptyState.Title>`) so it
   *  matches the surrounding document outline. */
  asChild?: boolean;
}

const EmptyStateTitle = forwardRef<HTMLHeadingElement, EmptyStateTitleProps>(
  function EmptyStateTitle({ asChild = false, className, children, ...rest }, ref) {
    if (asChild) {
      if (!isValidElement(children)) {
        if (process.env.NODE_ENV !== "production") {
          // eslint-disable-next-line no-console
          console.warn(
            "EmptyState.Title asChild expects a single React element child; received " +
              typeof children +
              "; rendering nothing.",
          );
        }
        return null;
      }
      return (
        <Slot
          {...rest}
          ref={ref as Ref<unknown>}
          data-slot="empty-state-title"
          className={classnames("zs-empty-state__title", className)}
        >
          {children}
        </Slot>
      );
    }
    return (
      <h2
        {...rest}
        ref={ref}
        data-slot="empty-state-title"
        className={classnames("zs-empty-state__title", className)}
      >
        {children}
      </h2>
    );
  },
);
EmptyStateTitle.displayName = "EmptyState.Title";

const EmptyStateDescription = forwardRef<
  HTMLParagraphElement,
  EmptyStateDescriptionProps
>(function EmptyStateDescription({ className, ...rest }, ref) {
  return (
    <p
      {...rest}
      ref={ref}
      data-slot="empty-state-description"
      className={classnames("zs-empty-state__description", className)}
    />
  );
});
EmptyStateDescription.displayName = "EmptyState.Description";

const EmptyStateActions = forwardRef<HTMLDivElement, EmptyStateActionsProps>(
  function EmptyStateActions({ className, ...rest }, ref) {
    return (
      <div
        {...rest}
        ref={ref}
        data-slot="empty-state-actions"
        className={classnames("zs-empty-state__actions", className)}
      />
    );
  },
);
EmptyStateActions.displayName = "EmptyState.Actions";

/* ─── public EmptyState namespace ────────────────────────────────────── */

type EmptyStateComponent = typeof EmptyStateRoot & {
  Icon: typeof EmptyStateIcon;
  Title: typeof EmptyStateTitle;
  Description: typeof EmptyStateDescription;
  Actions: typeof EmptyStateActions;
};

export const EmptyState = EmptyStateRoot as EmptyStateComponent;
EmptyState.Icon = EmptyStateIcon;
EmptyState.Title = EmptyStateTitle;
EmptyState.Description = EmptyStateDescription;
EmptyState.Actions = EmptyStateActions;
