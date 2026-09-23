import { Slider } from "@gather/meal-kit/components/ui/slider";
import { Label } from "@gather/meal-kit/components/ui/label";
import { useId } from "react";
import { Minus, Plus } from "lucide-react";
import { msg, plural } from "@lingui/core/macro";
import { useLingui } from "@lingui/react";
import { servingRange } from "@gather/meal-kit/domain";
import { Button } from "@gather/meal-kit/components/ui/button";

export function ServingSlider({
  value,
  onChange,
  label,
}: {
  value: number;
  onChange: (value: number) => void;
  label?: string;
}) {
  const { _: t } = useLingui();
  const id = useId();
  const count = t(
    msg({ message: plural(value, { one: "# person", other: "# people" }) }),
  );
  return (
    <div className="serving-control">
      <div className="flex items-center justify-between gap-4 mb-4">
        <Label htmlFor={id}>{label ?? t(msg`People per meal`)}</Label>
        <output htmlFor={id}>{count}</output>
      </div>
      <div className="serving-track">
        <Button
          type="button"
          variant="outline"
          size="icon"
          aria-label={t(msg`One fewer person`)}
          disabled={value <= servingRange.min}
          onClick={() => onChange(value - 1)}
        >
          <Minus size={16} />
        </Button>
        <Slider
          value={[value]}
          min={servingRange.min}
          max={servingRange.max}
          step={1}
          onValueChange={(next) =>
            onChange(Array.isArray(next) ? next[0] : next)
          }
          className="flex-1 py-4"
          thumbProps={{
            id,
            getAriaLabel: () => label ?? t(msg`People per meal`),
            getAriaValueText: () => count,
          }}
        />
        <Button
          type="button"
          variant="outline"
          size="icon"
          aria-label={t(msg`One more person`)}
          disabled={value >= servingRange.max}
          onClick={() => onChange(value + 1)}
        >
          <Plus size={16} />
        </Button>
      </div>
      <div className="range-limits" aria-hidden="true">
        <span>{servingRange.min}</span>
        <span>{servingRange.max}</span>
      </div>
    </div>
  );
}
