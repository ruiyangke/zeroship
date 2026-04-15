// node:events polyfill — EventEmitter.

type Listener = (...args: any[]) => void;

export class EventEmitter {
  private _events: Record<string, Listener[]> = {};
  private _maxListeners = 10;

  on(event: string, fn: Listener): this { (this._events[event] ??= []).push(fn); return this; }
  addListener(event: string, fn: Listener): this { return this.on(event, fn); }

  off(event: string, fn: Listener): this {
    const list = this._events[event];
    if (list) this._events[event] = list.filter(f => f !== fn);
    return this;
  }
  removeListener(event: string, fn: Listener): this { return this.off(event, fn); }

  once(event: string, fn: Listener): this {
    const wrapper: Listener = (...args) => { this.off(event, wrapper); fn(...args); };
    return this.on(event, wrapper);
  }

  emit(event: string, ...args: any[]): boolean {
    const list = this._events[event];
    if (!list || list.length === 0) return false;
    for (const fn of [...list]) fn(...args);
    return true;
  }

  removeAllListeners(event?: string): this {
    if (event) delete this._events[event];
    else this._events = {};
    return this;
  }

  listeners(event: string): Listener[] { return [...(this._events[event] ?? [])]; }
  rawListeners(event: string): Listener[] { return this.listeners(event); }
  listenerCount(event: string): number { return this._events[event]?.length ?? 0; }
  eventNames(): string[] { return Object.keys(this._events); }
  setMaxListeners(n: number): this { this._maxListeners = n; return this; }
  getMaxListeners(): number { return this._maxListeners; }

  prependListener(event: string, fn: Listener): this {
    (this._events[event] ??= []).unshift(fn);
    return this;
  }

  prependOnceListener(event: string, fn: Listener): this {
    const wrapper: Listener = (...args) => { this.off(event, wrapper); fn(...args); };
    return this.prependListener(event, wrapper);
  }

  static EventEmitter = EventEmitter;
  static defaultMaxListeners = 10;

  static once(emitter: EventEmitter, event: string): Promise<any[]> {
    return new Promise((resolve, reject) => {
      const onEvent = (...args: any[]) => { emitter.off("error", onError); resolve(args); };
      const onError = (err: any) => { emitter.off(event, onEvent); reject(err); };
      emitter.once(event, onEvent);
      emitter.once("error", onError);
    });
  }

  static on(emitter: EventEmitter, event: string): AsyncIterableIterator<any[]> {
    const queue: any[][] = [];
    let resolve: ((v: IteratorResult<any[]>) => void) | null = null;
    emitter.on(event, (...args) => {
      if (resolve) { resolve({ value: args, done: false }); resolve = null; }
      else queue.push(args);
    });
    return {
      [Symbol.asyncIterator]() { return this; },
      next() {
        if (queue.length > 0) return Promise.resolve({ value: queue.shift()!, done: false });
        return new Promise<IteratorResult<any[]>>(r => { resolve = r; });
      },
      return() { return Promise.resolve({ value: undefined, done: true }); },
      throw(err: any) { return Promise.reject(err); },
    };
  }
}

export default EventEmitter;
