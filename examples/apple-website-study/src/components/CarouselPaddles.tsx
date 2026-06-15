import type { RefObject } from "react";
import { ChevronLeft, ChevronRight } from "lucide-react";
import { Button, Icon } from "@zeroship/ui";

export function CarouselPaddles({
  label,
  target,
}: {
  label: string;
  target: RefObject<HTMLDivElement | null>;
}) {
  const scroll = (direction: -1 | 1) => {
    target.current?.scrollBy({
      left: direction * 420,
      behavior: "smooth",
    });
  };

  return (
    <div className="carousel-paddles" aria-label={label}>
      <Button
        className="carousel-paddle"
        variant="gray"
        size="small"
        aria-label="Previous"
        onClick={() => scroll(-1)}
      >
        <Icon as={ChevronLeft} size="sm" />
      </Button>
      <Button className="carousel-paddle" variant="gray" size="small" aria-label="Next" onClick={() => scroll(1)}>
        <Icon as={ChevronRight} size="sm" />
      </Button>
    </div>
  );
}
