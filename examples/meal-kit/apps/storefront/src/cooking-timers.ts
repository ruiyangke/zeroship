import type { MarketId } from "@gather/meal-kit/catalog";

export type TimerState = {
  duration: number;
  remaining: number;
  deadline: number | null;
  status: "running" | "paused" | "finished";
};
export type TimerContext = {
  key: string;
  step: number;
  recipeId: string;
  orderId?: string;
  market: MarketId;
  name: { en: string; zh: string };
};
export type CookingTimer = TimerContext & TimerState;

export function remainingTime(timer: TimerState, now: number) {
  return timer.deadline === null
    ? timer.remaining
    : Math.max(0, timer.deadline - now);
}
export function startTimer(duration: number, now: number): TimerState {
  if (!Number.isInteger(duration) || duration < 1000 || duration > 180 * 60_000)
    throw new RangeError("Invalid timer duration");
  return {
    duration,
    remaining: duration,
    deadline: now + duration,
    status: "running",
  };
}
export function pauseTimer(timer: TimerState, now: number): TimerState {
  const remaining = remainingTime(timer, now);
  return {
    ...timer,
    remaining,
    deadline: null,
    status: remaining ? "paused" : "finished",
  };
}
export function resumeTimer(timer: TimerState, now: number): TimerState {
  if (timer.status !== "paused") return timer;
  return { ...timer, deadline: now + timer.remaining, status: "running" };
}

export class CookingTimers {
  private timers: CookingTimer[] = [];
  private owner: string | null | undefined;
  private listeners = new Set<() => void>();
  private interval: ReturnType<typeof setInterval> | undefined;
  constructor(private now = () => Date.now()) {}
  getSnapshot = () => this.timers;
  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    if (!this.interval) this.interval = setInterval(() => this.tick(), 1000);
    return () => {
      this.listeners.delete(listener);
      if (!this.listeners.size) {
        clearInterval(this.interval);
        this.interval = undefined;
      }
    };
  };
  private publish(timers: CookingTimer[]) {
    this.timers = timers;
    for (const listener of this.listeners) listener();
  }
  setOwner(owner: string | null) {
    if (this.owner !== undefined && this.owner !== owner) this.publish([]);
    this.owner = owner;
  }
  start(context: TimerContext, duration: number) {
    this.publish([
      ...this.timers.filter((timer) => timer.key !== context.key),
      { ...context, ...startTimer(duration, this.now()) },
    ]);
  }
  pause(key: string) {
    this.publish(
      this.timers.map((timer) =>
        timer.key === key
          ? { ...timer, ...pauseTimer(timer, this.now()) }
          : timer,
      ),
    );
  }
  resume(key: string) {
    this.publish(
      this.timers.map((timer) =>
        timer.key === key
          ? { ...timer, ...resumeTimer(timer, this.now()) }
          : timer,
      ),
    );
  }
  remove(key: string) {
    this.publish(this.timers.filter((timer) => timer.key !== key));
  }
  tick() {
    if (!this.timers.some((timer) => timer.status === "running")) return;
    this.publish(
      this.timers.map((timer) => {
        if (timer.status !== "running") return timer;
        const remaining = remainingTime(timer, this.now());
        return {
          ...timer,
          remaining,
          deadline: remaining ? timer.deadline : null,
          status: remaining ? "running" : "finished",
        };
      }),
    );
  }
}

export const cookingTimers = new CookingTimers();
