(function () {
  const errorMonitor = Symbol.for("events.errorMonitor");
  const captureRejectionSymbol = Symbol.for("nodejs.rejection");
  let defaultMaxListeners = 10;
  let moduleCaptureRejections = false;

  function toKey(name) {
    return typeof name === "symbol" ? name : String(name);
  }

  function validateListener(fn) {
    if (typeof fn !== "function") throw new TypeError("listener must be a function");
  }

  function validateMaxListeners(n) {
    const value = Number(n);
    if (!Number.isFinite(value) || value < 0) {
      throw new RangeError("max listeners must be a non-negative number");
    }
    return value;
  }

  function ensureEvents(target) {
    if (target._events instanceof Map) return target._events;
    Object.defineProperty(target, "_events", {
      value: new Map(),
      enumerable: false,
      configurable: true,
      writable: true,
    });
    return target._events;
  }

  function EventEmitter(options = undefined) {
    const target = this && (typeof this === "object" || typeof this === "function")
      ? this
      : Object.create(EventEmitter.prototype);

    ensureEvents(target);
    Object.defineProperty(target, "_maxListeners", {
      value: undefined,
      enumerable: false,
      configurable: true,
      writable: true,
    });
    target.captureRejections = Boolean(
      options && options.captureRejections !== undefined
        ? options.captureRejections
        : moduleCaptureRejections,
    );

    return target;
  }

  EventEmitter.prototype._addListener = function _addListener(name, fn, prepend, original) {
    validateListener(fn);
    const events = ensureEvents(this);
    const key = toKey(name);
    const newListenerList = events.get("newListener");
    if (newListenerList && key !== "newListener") {
      for (const listener of [...newListenerList]) listener.call(this, name, original || fn);
    }
    const list = events.get(key);
    if (list) {
      if (prepend) list.unshift(fn);
      else list.push(fn);
    } else {
      events.set(key, [fn]);
    }
    return this;
  };

  EventEmitter.prototype.on = function on(name, fn) {
    return this._addListener(name, fn, false);
  };

  EventEmitter.prototype.addListener = function addListener(name, fn) {
    return this.on(name, fn);
  };

  EventEmitter.prototype.prependListener = function prependListener(name, fn) {
    return this._addListener(name, fn, true);
  };

  EventEmitter.prototype.once = function once(name, fn) {
    validateListener(fn);
    const self = this;
    function wrapped(...args) {
      self.removeListener(name, wrapped);
      return fn.apply(this, args);
    }
    wrapped.listener = fn;
    return this._addListener(name, wrapped, false, fn);
  };

  EventEmitter.prototype.prependOnceListener = function prependOnceListener(name, fn) {
    validateListener(fn);
    const self = this;
    function wrapped(...args) {
      self.removeListener(name, wrapped);
      return fn.apply(this, args);
    }
    wrapped.listener = fn;
    return this._addListener(name, wrapped, true, fn);
  };

  EventEmitter.prototype.emit = function emit(name, ...args) {
    const events = ensureEvents(this);
    const key = toKey(name);
    if (key === "error") {
      const monitors = events.get(errorMonitor);
      if (monitors) {
        for (const fn of [...monitors]) fn.apply(this, args);
      }
    }

    const list = events.get(key);
    if ((!list || list.length === 0) && key === "error") {
      const err = args[0];
      if (err instanceof Error) throw err;
      throw new Error(err == null ? "Unhandled error event" : String(err));
    }
    if (!list || list.length === 0) return false;
    for (const fn of [...list]) {
      const result = fn.apply(this, args);
      if (
        this.captureRejections &&
        result &&
        typeof result.then === "function" &&
        typeof result.catch === "function"
      ) {
        result.catch((err) => this.emit("error", err));
      }
    }
    return true;
  };

  EventEmitter.prototype.removeListener = function removeListener(name, fn) {
    validateListener(fn);
    const events = ensureEvents(this);
    const key = toKey(name);
    const list = events.get(key);
    if (!list) return this;
    const removed = [];
    const next = list.filter((f) => {
      const matched = f === fn || f.listener === fn;
      if (matched) removed.push(f.listener || f);
      return !matched;
    });
    if (next.length === 0) events.delete(key);
    else events.set(key, next);
    const removeList = events.get("removeListener");
    if (removeList && key !== "removeListener") {
      for (const listener of [...removeList]) {
        for (const removedListener of removed) listener.call(this, name, removedListener);
      }
    }
    return this;
  };

  EventEmitter.prototype.off = function off(name, fn) {
    return this.removeListener(name, fn);
  };

  EventEmitter.prototype.removeAllListeners = function removeAllListeners(name) {
    const events = ensureEvents(this);
    if (name === undefined) events.clear();
    else events.delete(toKey(name));
    return this;
  };

  EventEmitter.prototype.listenerCount = function listenerCount(name) {
    const list = ensureEvents(this).get(toKey(name));
    return list ? list.length : 0;
  };

  EventEmitter.prototype.listeners = function listeners(name) {
    return [...(ensureEvents(this).get(toKey(name)) || [])].map((fn) => fn.listener || fn);
  };

  EventEmitter.prototype.rawListeners = function rawListeners(name) {
    return [...(ensureEvents(this).get(toKey(name)) || [])];
  };

  EventEmitter.prototype.eventNames = function eventNames() {
    return [...ensureEvents(this).keys()].filter((key) => {
      const list = this._events.get(key);
      return list && list.length > 0;
    });
  };

  EventEmitter.prototype.setMaxListeners = function setMaxListeners(n) {
    this._maxListeners = validateMaxListeners(n);
    return this;
  };

  EventEmitter.prototype.getMaxListeners = function getMaxListeners() {
    return this._maxListeners === undefined ? defaultMaxListeners : this._maxListeners;
  };

  EventEmitter.prototype.constructor = EventEmitter;

  EventEmitter.listenerCount = function listenerCount(emitter, name) {
    return emitter.listenerCount(name);
  };

  EventEmitter.getEventListeners = function getEventListeners(emitter, name) {
    return emitter.listeners(name);
  };

  EventEmitter.setMaxListeners = function setMaxListeners(n, ...targets) {
    const value = validateMaxListeners(n);
    if (targets.length === 0) {
      defaultMaxListeners = value;
      return;
    }
    for (const target of targets) {
      if (target && typeof target.setMaxListeners === "function") {
        target.setMaxListeners(value);
      }
    }
  };

  EventEmitter.getMaxListeners = function getMaxListeners(emitter) {
    return emitter.getMaxListeners();
  };

  EventEmitter.errorMonitor = errorMonitor;
  EventEmitter.captureRejectionSymbol = captureRejectionSymbol;

  Object.defineProperty(EventEmitter, "defaultMaxListeners", {
    get() { return defaultMaxListeners; },
    set(value) { defaultMaxListeners = validateMaxListeners(value); },
    configurable: true,
  });
  Object.defineProperty(EventEmitter, "captureRejections", {
    get() { return moduleCaptureRejections; },
    set(value) { moduleCaptureRejections = Boolean(value); },
    configurable: true,
  });

  const ns = {
    EventEmitter,
    errorMonitor,
    captureRejectionSymbol,
    listenerCount: EventEmitter.listenerCount,
    getEventListeners: EventEmitter.getEventListeners,
    setMaxListeners: EventEmitter.setMaxListeners,
    getMaxListeners: EventEmitter.getMaxListeners,
  };
  Object.defineProperty(ns, "defaultMaxListeners", {
    get() { return EventEmitter.defaultMaxListeners; },
    set(value) { EventEmitter.defaultMaxListeners = value; },
    enumerable: true,
    configurable: true,
  });
  Object.defineProperty(ns, "captureRejections", {
    get() { return EventEmitter.captureRejections; },
    set(value) { EventEmitter.captureRejections = value; },
    enumerable: true,
    configurable: true,
  });
  ns.default = ns;
  globalThis.__zsEventEmitter = EventEmitter;
  return ns;
})()
