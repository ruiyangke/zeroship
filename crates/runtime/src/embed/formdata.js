(function(globalThis) {
"use strict";

function FormData() {
    this._entries = [];
}
FormData.prototype.append = function(name, value, filename) {
    if (value instanceof Blob && filename !== undefined) {
        value = new File([value], filename, { type: value.type });
    }
    this._entries.push([String(name), value]);
};
FormData.prototype.delete = function(name) {
    this._entries = this._entries.filter(function(e) { return e[0] !== name; });
};
FormData.prototype.get = function(name) {
    for (var i = 0; i < this._entries.length; i++) {
        if (this._entries[i][0] === name) return this._entries[i][1];
    }
    return null;
};
FormData.prototype.getAll = function(name) {
    return this._entries.filter(function(e) { return e[0] === name; }).map(function(e) { return e[1]; });
};
FormData.prototype.has = function(name) {
    return this._entries.some(function(e) { return e[0] === name; });
};
FormData.prototype.set = function(name, value, filename) {
    this.delete(name);
    this.append(name, value, filename);
};
FormData.prototype.entries = function() {
    var a = this._entries, i = 0;
    return { next: function() { return i >= a.length ? { done: true } : { done: false, value: a[i++].slice() }; }, [Symbol.iterator]: function() { return this; } };
};
FormData.prototype.keys = function() {
    var a = this._entries, i = 0;
    return { next: function() { return i >= a.length ? { done: true } : { done: false, value: a[i++][0] }; }, [Symbol.iterator]: function() { return this; } };
};
FormData.prototype.values = function() {
    var a = this._entries, i = 0;
    return { next: function() { return i >= a.length ? { done: true } : { done: false, value: a[i++][1] }; }, [Symbol.iterator]: function() { return this; } };
};
FormData.prototype.forEach = function(cb, thisArg) {
    for (var i = 0; i < this._entries.length; i++) {
        cb.call(thisArg, this._entries[i][1], this._entries[i][0], this);
    }
};
FormData.prototype[Symbol.iterator] = function() { return this.entries(); };

globalThis.FormData = FormData;

})(globalThis);
