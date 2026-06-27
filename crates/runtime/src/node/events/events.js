(function () {
  function EventEmitter() {
    const target = this && (typeof this === "object" || typeof this === "function")
      ? this
      : Object.create(EventEmitter.prototype);

    Object.defineProperty(target, "_events", {
      value: new Map(),
      enumerable: false,
      configurable: true,
      writable: true,
    });

    return target;
  }

  EventEmitter.prototype.on = function on(name, fn) {
    if (typeof fn !== "function") throw new TypeError("listener must be a function");
    const key = String(name);
    const list = this._events.get(key);
    if (list) list.push(fn);
    else this._events.set(key, [fn]);
    return this;
  };

  EventEmitter.prototype.addListener = function addListener(name, fn) {
    return this.on(name, fn);
  };

  EventEmitter.prototype.prependListener = function prependListener(name, fn) {
    if (typeof fn !== "function") throw new TypeError("listener must be a function");
    const key = String(name);
    const list = this._events.get(key);
    if (list) list.unshift(fn);
    else this._events.set(key, [fn]);
    return this;
  };

  EventEmitter.prototype.once = function once(name, fn) {
    if (typeof fn !== "function") throw new TypeError("listener must be a function");
    const self = this;
    function wrapped(...args) {
      self.removeListener(name, wrapped);
      return fn.apply(this, args);
    }
    wrapped.listener = fn;
    return this.on(name, wrapped);
  };

  EventEmitter.prototype.prependOnceListener = function prependOnceListener(name, fn) {
    if (typeof fn !== "function") throw new TypeError("listener must be a function");
    const self = this;
    function wrapped(...args) {
      self.removeListener(name, wrapped);
      return fn.apply(this, args);
    }
    wrapped.listener = fn;
    return this.prependListener(name, wrapped);
  };

  EventEmitter.prototype.emit = function emit(name, ...args) {
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
  };

  EventEmitter.prototype.removeListener = function removeListener(name, fn) {
    const key = String(name);
    const list = this._events.get(key);
    if (!list) return this;
    const next = list.filter((f) => f !== fn && f.listener !== fn);
    if (next.length === 0) this._events.delete(key);
    else this._events.set(key, next);
    return this;
  };

  EventEmitter.prototype.off = function off(name, fn) {
    return this.removeListener(name, fn);
  };

  EventEmitter.prototype.removeAllListeners = function removeAllListeners(name) {
    if (name === undefined) this._events.clear();
    else this._events.delete(String(name));
    return this;
  };

  EventEmitter.prototype.listenerCount = function listenerCount(name) {
    const list = this._events.get(String(name));
    return list ? list.length : 0;
  };

  EventEmitter.prototype.listeners = function listeners(name) {
    return [...(this._events.get(String(name)) || [])];
  };

  EventEmitter.prototype.constructor = EventEmitter;

  EventEmitter.listenerCount = function listenerCount(emitter, name) {
    return emitter.listenerCount(name);
  };

  const ns = { EventEmitter };
  ns.default = ns;
  globalThis.__zsEventEmitter = EventEmitter;
  return ns;
})()
