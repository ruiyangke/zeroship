(function () {
  class EventEmitter {
    constructor() {
      Object.defineProperty(this, "_events", {
        value: new Map(),
        enumerable: false,
        configurable: true,
        writable: true,
      });
    }

    on(name, fn) {
      if (typeof fn !== "function") throw new TypeError("listener must be a function");
      const key = String(name);
      const list = this._events.get(key);
      if (list) list.push(fn);
      else this._events.set(key, [fn]);
      return this;
    }

    addListener(name, fn) {
      return this.on(name, fn);
    }

    once(name, fn) {
      if (typeof fn !== "function") throw new TypeError("listener must be a function");
      const self = this;
      function wrapped(...args) {
        self.removeListener(name, wrapped);
        return fn.apply(this, args);
      }
      wrapped.listener = fn;
      return this.on(name, wrapped);
    }

    emit(name, ...args) {
      const key = String(name);
      const list = this._events.get(key);
      if ((!list || list.length === 0) && key === "error") {
        const err = args[0];
        if (err instanceof Error) throw err;
        throw new Error(err == null ? "Unhandled error event" : String(err));
      }
      if (!list || list.length === 0) return false;
      for (const fn of [...list]) fn.apply(this, args);
      return true;
    }

    removeListener(name, fn) {
      const key = String(name);
      const list = this._events.get(key);
      if (!list) return this;
      const next = list.filter((f) => f !== fn && f.listener !== fn);
      if (next.length === 0) this._events.delete(key);
      else this._events.set(key, next);
      return this;
    }

    off(name, fn) {
      return this.removeListener(name, fn);
    }

    removeAllListeners(name) {
      if (name === undefined) this._events.clear();
      else this._events.delete(String(name));
      return this;
    }

    listenerCount(name) {
      const list = this._events.get(String(name));
      return list ? list.length : 0;
    }

    listeners(name) {
      return [...(this._events.get(String(name)) || [])];
    }
  }

  EventEmitter.listenerCount = function listenerCount(emitter, name) {
    return emitter.listenerCount(name);
  };

  const ns = { EventEmitter };
  ns.default = ns;
  globalThis.__zsEventEmitter = EventEmitter;
  return ns;
})()
