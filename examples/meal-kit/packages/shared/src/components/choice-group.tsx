import { useId } from "react";
import { RadioGroup, RadioGroupItem } from "@gather/meal-kit/components/ui/radio-group";
import { FieldSet, FieldLegend, FieldDescription } from "@gather/meal-kit/components/ui/field";
import { Label } from "@gather/meal-kit/components/ui/label";

export function ChoiceGroup<T extends string | number>({
  label,
  value,
  onChange,
  options,
  columns = false,
  hint,
}: {
  label: string;
  value: T;
  onChange: (value: T) => void;
  options: { value: T; label: string; description?: string }[];
  columns?: boolean;
  hint?: string;
}) {
  const name = useId();
  return (
    <FieldSet
      className="choice-group"
      aria-describedby={hint ? `${name}-hint` : undefined}
    >
      <FieldLegend id={`${name}-label`}>{label}</FieldLegend>
      {hint && <FieldDescription id={`${name}-hint`}>{hint}</FieldDescription>}
      <RadioGroup
        aria-labelledby={`${name}-label`}
        value={String(value)}
        onValueChange={(next) => {
          const option = options.find(
            (option) => String(option.value) === next,
          );
          if (option) onChange(option.value);
        }}
        className={columns ? "choice-options choice-columns" : "choice-options"}
      >
        {options.map((option) => (
          <Label
            htmlFor={`${name}-${option.value}`}
            key={option.value}
            className="choice-card"
          >
            <span className="choice-copy">
              <strong>{option.label}</strong>
              {option.description && <span>{option.description}</span>}
            </span>
            <RadioGroupItem
              id={`${name}-${option.value}`}
              value={String(option.value)}
            />
          </Label>
        ))}
      </RadioGroup>
    </FieldSet>
  );
}
