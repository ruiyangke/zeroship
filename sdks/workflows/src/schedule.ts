const SCHEDULE_BRAND = Symbol("zeroship.schedule");
const REGISTRATION_BRAND = Symbol("zeroship.schedule.registration");

export type IntervalUnit = "seconds" | "minutes" | "hours" | "days";
export type ScheduleOverlap = "allow" | "skipIfRunning";
export type ScheduleCatchUp =
  | { readonly mode: "skip" }
  | { readonly mode: "backfill"; readonly max: number };
export type ScheduleAnchor = "epoch" | "deploy";

type Digit = "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9";
export type TimeOfDay = `${Digit}${Digit}:${Digit}${Digit}`;
export type TimeZone = string;

interface SchedulePolicyFields {
  readonly overlap: ScheduleOverlap;
  readonly catchUp: ScheduleCatchUp;
}

export type CronScheduleDescriptor = SchedulePolicyFields & {
  readonly kind: "cron";
  readonly cron_expr: string;
  readonly tz: TimeZone;
};

export type IntervalScheduleDescriptor = SchedulePolicyFields & {
  readonly kind: "interval";
  readonly interval_ms: number;
  readonly anchor: ScheduleAnchor;
};

export type NormalizedScheduleDescriptor =
  | CronScheduleDescriptor
  | IntervalScheduleDescriptor;

export type Schedule = NormalizedScheduleDescriptor & {
  readonly [SCHEDULE_BRAND]: true;
};

export interface CompileScheduleOptions {
  readonly overlap?: ScheduleOverlap;
  readonly catchUp?: ScheduleCatchUp;
}

type WorkflowInstance<P = unknown, O = unknown> = {
  run(trigger: { input: P }, ...args: any[]): O | Promise<O>;
};
type WorkflowClass<P = unknown, O = unknown> = new () => WorkflowInstance<P, O>;
type ParamsOf<W extends WorkflowClass<any, any>> =
  InstanceType<W> extends { run(trigger: infer T, ...args: any[]): any }
    ? T extends { input: infer P }
      ? P
      : unknown
    : unknown;
type InputField<P> = [P] extends [void | undefined]
  ? { readonly input?: P }
  : { readonly input: P };

export type ScheduleRegistrationOptions<W extends WorkflowClass<any, any>> = {
  readonly name: string;
  readonly schedule: Schedule | string;
  readonly workflow: W;
  readonly overlap?: ScheduleOverlap;
  readonly catchUp?: ScheduleCatchUp;
} & InputField<ParamsOf<W>>;

export interface ScheduleRegistration<
  Input = unknown,
> {
  readonly name: string;
  readonly workflowName: string;
  readonly schedule: Schedule;
  readonly input: Input;
  readonly overlap: ScheduleOverlap;
  readonly catchUp: ScheduleCatchUp;
  readonly [REGISTRATION_BRAND]: true;
}

interface DayCadence {
  at(time: TimeOfDay, tz?: TimeZone): Schedule;
}

interface WeekdayCadence {
  at(time: TimeOfDay, tz?: TimeZone): Schedule;
}

type HourCadence = Schedule & {
  at(minute: number): Schedule;
};

interface MonthCadence {
  on(dayOfMonth: number): { at(time: TimeOfDay, tz?: TimeZone): Schedule };
}

interface CountCadence {
  seconds(): Schedule;
  minutes(): Schedule;
  hours(): Schedule;
  days(): Schedule;
}

interface EmptyCadence {
  minute(): Schedule;
  hour(): HourCadence;
  day(): DayCadence;
  month(): MonthCadence;
  sunday(): WeekdayCadence;
  monday(): WeekdayCadence;
  tuesday(): WeekdayCadence;
  wednesday(): WeekdayCadence;
  thursday(): WeekdayCadence;
  friday(): WeekdayCadence;
  saturday(): WeekdayCadence;
}

interface Every {
  (n: number, unit: IntervalUnit): Schedule;
  (n: number): CountCadence;
  (): EmptyCadence;
  minute(): Schedule;
  hour(): HourCadence;
  readonly day: DayCadence;
  readonly month: MonthCadence;
  readonly sunday: WeekdayCadence;
  readonly monday: WeekdayCadence;
  readonly tuesday: WeekdayCadence;
  readonly wednesday: WeekdayCadence;
  readonly thursday: WeekdayCadence;
  readonly friday: WeekdayCadence;
  readonly saturday: WeekdayCadence;
}

export class InvalidScheduleError extends Error {
  constructor(message = "invalid workflow schedule") {
    super(message);
    this.name = "InvalidScheduleError";
  }
}

const DEFAULT_TZ = "UTC";
const DEFAULT_OVERLAP: ScheduleOverlap = "allow";
const DEFAULT_CATCH_UP: ScheduleCatchUp = Object.freeze({ mode: "skip" });
const CRON_MACROS: Record<string, string> = Object.freeze({
  "@hourly": "0 * * * *",
  "@daily": "0 0 * * *",
  "@weekly": "0 0 * * 0",
  "@monthly": "0 0 1 * *",
  "@yearly": "0 0 1 1 *",
});
const UNIT_MS: Record<IntervalUnit, number> = Object.freeze({
  seconds: 1_000,
  minutes: 60_000,
  hours: 3_600_000,
  days: 86_400_000,
});
const WEEKDAYS: Record<string, number> = Object.freeze({
  sunday: 0,
  monday: 1,
  tuesday: 2,
  wednesday: 3,
  thursday: 4,
  friday: 5,
  saturday: 6,
});

export const every: Every = Object.assign(
  ((n?: number, unit?: IntervalUnit) => {
    if (n === undefined) return emptyCadence();
    if (unit === undefined) return countCadence(n);
    return intervalSchedule(n, unit);
  }) as Every,
  {
    minute: () => cronSchedule("* * * * *", DEFAULT_TZ),
    hour: () => hourCadence(0),
    day: dayCadence("*"),
    month: monthCadence(),
    sunday: weekdayCadence(WEEKDAYS.sunday),
    monday: weekdayCadence(WEEKDAYS.monday),
    tuesday: weekdayCadence(WEEKDAYS.tuesday),
    wednesday: weekdayCadence(WEEKDAYS.wednesday),
    thursday: weekdayCadence(WEEKDAYS.thursday),
    friday: weekdayCadence(WEEKDAYS.friday),
    saturday: weekdayCadence(WEEKDAYS.saturday),
  },
);

export function cronExpr(expr: string, tz: TimeZone = DEFAULT_TZ): Schedule {
  return cronSchedule(expr, tz);
}

export function compileSchedule(
  input: Schedule | string,
  options: CompileScheduleOptions = {},
): Schedule {
  const overlap = normalizeOverlap(options.overlap);
  const catchUp = normalizeCatchUp(options.catchUp);

  if (typeof input === "string") {
    return cronSchedule(input, DEFAULT_TZ, { overlap, catchUp });
  }

  if (!isScheduleLike(input)) {
    throw new InvalidScheduleError("schedule must be a fluent schedule or a 5-field cron string");
  }

  if (input.kind === "cron") {
    return cronSchedule(input.cron_expr, input.tz, { overlap, catchUp });
  }
  return intervalScheduleFromMs(input.interval_ms, input.anchor, { overlap, catchUp });
}

export function schedule<W extends WorkflowClass<any, any>>(
  opts: ScheduleRegistrationOptions<W>,
): ScheduleRegistration<ParamsOf<W>> {
  if (!opts || typeof opts !== "object") {
    throw new InvalidScheduleError("schedule registration options are required");
  }
  if (typeof opts.name !== "string" || opts.name.trim() === "") {
    throw new InvalidScheduleError("schedule name must be a non-empty string");
  }
  const workflowName = opts.workflow.name;
  if (typeof workflowName !== "string" || workflowName.length === 0) {
    throw new InvalidScheduleError("schedule workflow must be a named Workflow class");
  }
  const compiled = compileSchedule(opts.schedule, {
    overlap: opts.overlap,
    catchUp: opts.catchUp,
  });
  const registration = {
    name: opts.name,
    workflowName,
    schedule: compiled,
    input: "input" in opts ? opts.input : {},
    overlap: compiled.overlap,
    catchUp: compiled.catchUp,
  } as ScheduleRegistration<ParamsOf<W>>;
  return brandAndFreeze(registration, REGISTRATION_BRAND);
}

function emptyCadence(): EmptyCadence {
  return {
    minute: () => every.minute(),
    hour: () => every.hour(),
    day: () => every.day,
    month: () => every.month,
    sunday: () => every.sunday,
    monday: () => every.monday,
    tuesday: () => every.tuesday,
    wednesday: () => every.wednesday,
    thursday: () => every.thursday,
    friday: () => every.friday,
    saturday: () => every.saturday,
  };
}

function countCadence(n: number): CountCadence {
  return {
    seconds: () => intervalSchedule(n, "seconds"),
    minutes: () => intervalSchedule(n, "minutes"),
    hours: () => intervalSchedule(n, "hours"),
    days: () => intervalSchedule(n, "days"),
  };
}

function dayCadence(dayOfWeek: "*" | number): DayCadence {
  return {
    at(time: TimeOfDay, tz: TimeZone = DEFAULT_TZ) {
      const { hour, minute } = parseTimeOfDay(time);
      return cronSchedule(`${minute} ${hour} * * ${dayOfWeek}`, tz);
    },
  };
}

function weekdayCadence(dayOfWeek: number): WeekdayCadence {
  return dayCadence(dayOfWeek);
}

function hourCadence(minute: number): HourCadence {
  const schedule: CronScheduleDescriptor = {
    kind: "cron",
    cron_expr: `${normalizeMinute(minute)} * * * *`,
    tz: DEFAULT_TZ,
    overlap: DEFAULT_OVERLAP,
    catchUp: DEFAULT_CATCH_UP,
  };
  Object.defineProperty(schedule, "at", {
    value: (m: number) => cronSchedule(`${normalizeMinute(m)} * * * *`, DEFAULT_TZ),
    enumerable: false,
  });
  return brandAndFreeze(schedule, SCHEDULE_BRAND) as HourCadence;
}

function monthCadence(): MonthCadence {
  return {
    on(dayOfMonth: number) {
      const day = normalizeDayOfMonth(dayOfMonth);
      return {
        at(time: TimeOfDay, tz: TimeZone = DEFAULT_TZ) {
          const { hour, minute } = parseTimeOfDay(time);
          return cronSchedule(`${minute} ${hour} ${day} * *`, tz);
        },
      };
    },
  };
}

function cronSchedule(
  expr: string,
  tz: TimeZone,
  policies?: SchedulePolicyFields,
): Schedule {
  const normalized = normalizeCronExpr(expr);
  validateTimeZone(tz);
  return scheduleDescriptor({
    kind: "cron",
    cron_expr: normalized,
    tz,
    overlap: normalizeOverlap(policies?.overlap),
    catchUp: normalizeCatchUp(policies?.catchUp),
  });
}

function intervalSchedule(
  n: number,
  unit: IntervalUnit,
  policies?: SchedulePolicyFields,
): Schedule {
  if (!(unit in UNIT_MS)) {
    throw new InvalidScheduleError(`unsupported interval unit: ${String(unit)}`);
  }
  if (!Number.isInteger(n) || n <= 0) {
    throw new InvalidScheduleError("interval count must be a positive integer");
  }
  return intervalScheduleFromMs(n * UNIT_MS[unit], "epoch", policies);
}

function intervalScheduleFromMs(
  intervalMs: number,
  anchor: ScheduleAnchor,
  policies?: SchedulePolicyFields,
): Schedule {
  if (!Number.isSafeInteger(intervalMs) || intervalMs < 1_000) {
    throw new InvalidScheduleError("interval_ms must be an integer number of milliseconds, at least 1000");
  }
  if (anchor !== "epoch" && anchor !== "deploy") {
    throw new InvalidScheduleError(`unsupported interval anchor: ${String(anchor)}`);
  }
  return scheduleDescriptor({
    kind: "interval",
    interval_ms: intervalMs,
    anchor,
    overlap: normalizeOverlap(policies?.overlap),
    catchUp: normalizeCatchUp(policies?.catchUp),
  });
}

function scheduleDescriptor<T extends NormalizedScheduleDescriptor>(descriptor: T): T & Schedule {
  return brandAndFreeze(descriptor, SCHEDULE_BRAND) as T & Schedule;
}

function brandAndFreeze<T extends object, B extends symbol>(
  value: T,
  brand: B,
): T & { readonly [K in B]: true } {
  Object.defineProperty(value, brand, {
    value: true,
    enumerable: false,
  });
  return Object.freeze(value) as T & { readonly [K in B]: true };
}

function isScheduleLike(value: unknown): value is NormalizedScheduleDescriptor {
  if (!value || typeof value !== "object") return false;
  const record = value as Record<string, unknown>;
  return record.kind === "cron" || record.kind === "interval";
}

function normalizeOverlap(value: ScheduleOverlap | undefined): ScheduleOverlap {
  if (value === undefined) return DEFAULT_OVERLAP;
  if (value === "allow" || value === "skipIfRunning") return value;
  throw new InvalidScheduleError(`unsupported overlap policy: ${String(value)}`);
}

function normalizeCatchUp(value: ScheduleCatchUp | undefined): ScheduleCatchUp {
  if (value === undefined) return DEFAULT_CATCH_UP;
  if (!value || typeof value !== "object") {
    throw new InvalidScheduleError("catchUp must be { mode: \"skip\" } or { mode: \"backfill\", max }");
  }
  const record = value as { mode?: unknown; max?: unknown };
  if (record.mode === "skip") return DEFAULT_CATCH_UP;
  if (record.mode !== "backfill") {
    throw new InvalidScheduleError(`unsupported catchUp mode: ${String(record.mode)}`);
  }
  if (!Number.isInteger(record.max) || Number(record.max) <= 0) {
    throw new InvalidScheduleError("catchUp backfill max must be a positive integer");
  }
  return Object.freeze({ mode: "backfill", max: Number(record.max) });
}

function normalizeCronExpr(expr: string): string {
  if (typeof expr !== "string" || expr.trim() === "") {
    throw new InvalidScheduleError("cron expression must be a non-empty string");
  }
  const compact = expr.trim().replace(/\s+/g, " ");
  const macro = CRON_MACROS[compact];
  const normalized = macro ?? compact;
  if (normalized.startsWith("@")) {
    throw new InvalidScheduleError(`unsupported cron macro: ${normalized}`);
  }
  const fields = normalized.split(" ");
  if (fields.length === 6) {
    throw new InvalidScheduleError("sub-minute cron is unsupported; use a 5-field expression");
  }
  if (fields.length !== 5) {
    throw new InvalidScheduleError("cron expression must have exactly 5 fields");
  }
  validateCronField(fields[0], "minute", 0, 59);
  validateCronField(fields[1], "hour", 0, 23);
  validateCronField(fields[2], "day-of-month", 1, 31, { rejectExplicitAbove: 28 });
  validateCronField(fields[3], "month", 1, 12);
  validateCronField(fields[4], "day-of-week", 0, 7);
  return normalized;
}

function validateCronField(
  field: string,
  label: string,
  min: number,
  max: number,
  opts: { rejectExplicitAbove?: number } = {},
): void {
  if (field === "") {
    throw new InvalidScheduleError(`cron ${label} field is empty`);
  }
  if (/[A-Za-z?#LW]/.test(field)) {
    throw new InvalidScheduleError(`cron ${label} field contains an unsupported token`);
  }
  for (const part of field.split(",")) {
    validateCronPart(part, label, min, max, opts);
  }
}

function validateCronPart(
  part: string,
  label: string,
  min: number,
  max: number,
  opts: { rejectExplicitAbove?: number },
): void {
  if (part === "") {
    throw new InvalidScheduleError(`cron ${label} field contains an empty list item`);
  }
  const [base, step, extra] = part.split("/");
  if (extra !== undefined) {
    throw new InvalidScheduleError(`cron ${label} field has a malformed step`);
  }
  if (step !== undefined) {
    const stepValue = parseCronInt(step, label);
    if (stepValue <= 0) {
      throw new InvalidScheduleError(`cron ${label} step must be positive`);
    }
    if (label === "day-of-month" && base === "*") {
      throw new InvalidScheduleError("day-of-month stepped wildcard is unsupported");
    }
  }
  if (base === "*") return;
  const range = base.split("-");
  if (range.length > 2 || range[0] === "" || range[1] === "") {
    throw new InvalidScheduleError(`cron ${label} field has a malformed range`);
  }
  const start = parseCronInt(range[0], label);
  const end = range.length === 2 ? parseCronInt(range[1], label) : start;
  if (start < min || start > max || end < min || end > max || start > end) {
    throw new InvalidScheduleError(`cron ${label} field is out of range`);
  }
  if (opts.rejectExplicitAbove !== undefined && end > opts.rejectExplicitAbove) {
    throw new InvalidScheduleError("day-of-month values above 28 are unsupported");
  }
}

function parseCronInt(value: string, label: string): number {
  if (!/^\d+$/.test(value)) {
    throw new InvalidScheduleError(`cron ${label} field must use integers`);
  }
  return Number(value);
}

function parseTimeOfDay(value: TimeOfDay): { hour: number; minute: number } {
  if (typeof value !== "string" || !/^\d\d:\d\d$/.test(value)) {
    throw new InvalidScheduleError("time must be formatted as HH:MM");
  }
  const hour = Number(value.slice(0, 2));
  const minute = Number(value.slice(3, 5));
  if (hour < 0 || hour > 23 || minute < 0 || minute > 59) {
    throw new InvalidScheduleError("time must be a valid 24-hour HH:MM value");
  }
  return { hour, minute };
}

function normalizeMinute(value: number): number {
  if (!Number.isInteger(value) || value < 0 || value > 59) {
    throw new InvalidScheduleError("minute must be an integer from 0 through 59");
  }
  return value;
}

function normalizeDayOfMonth(value: number): number {
  if (!Number.isInteger(value) || value < 1 || value > 28) {
    throw new InvalidScheduleError("day of month must be an integer from 1 through 28");
  }
  return value;
}

function validateTimeZone(tz: TimeZone): void {
  if (typeof tz !== "string" || tz.trim() === "") {
    throw new InvalidScheduleError("timezone must be a non-empty IANA name");
  }
  try {
    new Intl.DateTimeFormat("en-US", { timeZone: tz }).format(0);
  } catch {
    throw new InvalidScheduleError(`unknown IANA timezone: ${tz}`);
  }
}
