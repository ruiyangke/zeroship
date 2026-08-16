import { Avatar as BaseAvatar } from "@base-ui/react/avatar";
import { forwardRef, type ReactNode } from "react";

export type AvatarSize = "xs" | "sm" | "md" | "lg" | "xl";
export type AvatarShape = "circle" | "square" | "rounded";

type AvatarPropsBase = Omit<BaseAvatar.Root.Props, "children"> & {
  size?: AvatarSize;
  shape?: AvatarShape;
  fallback?: ReactNode;
  fallbackDelay?: number;
  children?: ReactNode;
};

type AvatarPropsWithSrc = AvatarPropsBase & {
  src: string;
  alt: string;
};

type AvatarPropsWithoutSrc = AvatarPropsBase & {
  src?: undefined;
  alt?: undefined;
};

export type AvatarProps = AvatarPropsWithSrc | AvatarPropsWithoutSrc;

export const Avatar = forwardRef<HTMLSpanElement, AvatarProps>(function Avatar(
  props,
  ref,
) {
  const {
    src,
    alt,
    fallback,
    fallbackDelay,
    children,
    size = "md",
    shape = "circle",
    ...rootProps
  } = props;

  const fallbackA11y =
    src === undefined
      ? {}
      : alt === ""
        ? { "aria-hidden": true as const }
        : { role: "img" as const, "aria-label": alt };

  return (
    <BaseAvatar.Root
      {...rootProps}
      ref={ref}
      data-slot="avatar"
      data-size={size}
      data-shape={shape}
    >
      {children ?? (
        <>
          {src !== undefined ? (
            <BaseAvatar.Image
              src={src}
              alt={alt}
              data-slot="avatar-image"
            />
          ) : null}
          <BaseAvatar.Fallback
            delay={fallbackDelay}
            data-slot="avatar-fallback"
            {...fallbackA11y}
          >
            {fallback}
          </BaseAvatar.Fallback>
        </>
      )}
    </BaseAvatar.Root>
  );
});

Avatar.displayName = "Avatar";
