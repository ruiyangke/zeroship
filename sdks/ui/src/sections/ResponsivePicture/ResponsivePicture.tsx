/*
 * ResponsivePicture — breakpoint-aware media with governed crop behavior.
 *
 * This is intentionally a media primitive, not a gallery component. It wraps
 * the native <picture>/<source>/<img> shape and adds the design-system pieces
 * product pages need: aspect-ratio, object-fit, object-position, radius, and
 * an optional surface frame.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type CSSProperties,
} from "react";
import { classnames } from "../../components/_classnames";

export interface ResponsivePictureSource {
  srcSet: string;
  media?: string;
  type?: string;
  sizes?: string;
}

export type ResponsivePictureRatio =
  | "auto"
  | "square"
  | "video"
  | "wide"
  | "banner"
  | "portrait"
  | "product";
export type ResponsivePictureFit = "cover" | "contain";
export type ResponsivePicturePosition =
  | "center"
  | "top"
  | "bottom"
  | "left"
  | "right";
export type ResponsivePictureRadius = "none" | "sm" | "md" | "lg" | "xl";
export type ResponsivePictureFrame = "none" | "clip" | "surface";

export interface ResponsivePictureProps
  extends Omit<
    ComponentPropsWithoutRef<"img">,
    "alt" | "className" | "src" | "srcSet" | "sizes"
  > {
  src: string;
  alt: string;
  sources?: ResponsivePictureSource[];
  sizes?: string;
  ratio?: ResponsivePictureRatio;
  fit?: ResponsivePictureFit;
  position?: ResponsivePicturePosition;
  radius?: ResponsivePictureRadius;
  frame?: ResponsivePictureFrame;
  className?: string;
  imageClassName?: string;
  /** Root `data-slot` value. Defaults to `"responsive-picture"`. */
  "data-slot"?: string;
}

const POSITION: Record<ResponsivePicturePosition, string> = {
  center: "center",
  top: "center top",
  bottom: "center bottom",
  left: "left center",
  right: "right center",
};

export const ResponsivePicture = forwardRef<
  HTMLPictureElement,
  ResponsivePictureProps
>(function ResponsivePicture(
  {
    src,
    alt,
    sources,
    sizes,
    ratio = "auto",
    fit = "cover",
    position = "center",
    radius = "md",
    frame = "clip",
    className,
    imageClassName,
    style,
    "data-slot": dataSlot = "responsive-picture",
    ...rest
  },
  ref,
) {
  const imageStyle: CSSProperties = {
    objectPosition: POSITION[position],
    ...style,
  };

  return (
    <picture
      ref={ref}
      data-slot={dataSlot}
      data-ratio={ratio}
      data-fit={fit}
      data-radius={radius}
      data-frame={frame}
      className={classnames("zs-responsive-picture", className)}
    >
      {sources?.map((source) => (
        <source
          key={`${source.media ?? "default"}:${source.type ?? ""}:${source.srcSet}`}
          media={source.media}
          srcSet={source.srcSet}
          type={source.type}
          sizes={source.sizes}
        />
      ))}
      <img
        {...rest}
        src={src}
        alt={alt}
        sizes={sizes}
        className={classnames("zs-responsive-picture__img", imageClassName)}
        style={imageStyle}
      />
    </picture>
  );
});
ResponsivePicture.displayName = "ResponsivePicture";
