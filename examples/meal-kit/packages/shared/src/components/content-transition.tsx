import { useLayoutEffect, useRef, type ReactNode } from "react";
import { useReducedMotion } from "motion/react";
import { useAnimate } from "motion/react-mini";

export const contentTiming = {
  duration: 0.2,
  ease: [0.22, 1, 0.36, 1] as [number, number, number, number],
};

export function ContentTransition({
  change,
  kind = "panel",
  direction = 1,
  enter = false,
  children,
}: {
  change: string;
  kind?: "page" | "wizard" | "panel";
  direction?: 1 | -1;
  enter?: boolean;
  children: ReactNode;
}) {
  const [scope, animate] = useAnimate<HTMLDivElement>();
  const reduce = useReducedMotion();
  const previous = useRef<string | null>(enter ? null : change);

  useLayoutEffect(() => {
    const changed = previous.current !== change;
    previous.current = change;
    if (!changed || reduce) return;
    const node = scope.current;
    const rtl = document.documentElement.dir === "rtl" ? -1 : 1;
    const transform =
      kind === "wizard"
        ? `translateX(${direction * rtl * 12}px)`
        : "translateY(6px)";
    const animation = animate(
      node,
      kind === "page"
        ? { opacity: [0.65, 1] }
        : { opacity: [0.65, 1], transform: [transform, "none"] },
      contentTiming,
    );
    return () => {
      animation.stop();
      node.style.removeProperty("opacity");
      node.style.removeProperty("transform");
    };
  }, [change, reduce, kind, direction, animate, scope]);

  return (
    <div ref={scope} data-transition={kind}>
      {children}
    </div>
  );
}
