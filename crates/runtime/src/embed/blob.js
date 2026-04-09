(function(globalThis) {
"use strict";

function Blob(parts, options) {
    options = options || {};
    this.type = options.type ? String(options.type).toLowerCase() : "";

    // Concatenate all parts into a single Uint8Array
    var buffers = [];
    var totalLen = 0;
    if (parts) {
        for (var i = 0; i < parts.length; i++) {
            var part = parts[i];
            var buf;
            if (part instanceof Uint8Array) {
                buf = part;
            } else if (part instanceof ArrayBuffer) {
                buf = new Uint8Array(part);
            } else if (part instanceof Blob) {
                buf = part._data;
            } else {
                // String
                buf = new TextEncoder().encode(String(part));
            }
            buffers.push(buf);
            totalLen += buf.length;
        }
    }
    var data = new Uint8Array(totalLen);
    var offset = 0;
    for (var j = 0; j < buffers.length; j++) {
        data.set(buffers[j], offset);
        offset += buffers[j].length;
    }
    this._data = data;
    this.size = data.length;
}

Blob.prototype.text = function() {
    return Promise.resolve(new TextDecoder().decode(this._data));
};
Blob.prototype.arrayBuffer = function() {
    return Promise.resolve(this._data.buffer.slice(0));
};
Blob.prototype.slice = function(start, end, type) {
    start = start || 0;
    end = end === undefined ? this.size : end;
    if (start < 0) start = Math.max(this.size + start, 0);
    if (end < 0) end = Math.max(this.size + end, 0);
    var sliced = this._data.slice(start, end);
    var b = new Blob([], { type: type || this.type });
    b._data = sliced;
    b.size = sliced.length;
    return b;
};
Blob.prototype.stream = function() {
    var data = this._data;
    return new ReadableStream({
        start: function(controller) {
            controller.enqueue(data);
            controller.close();
        }
    });
};

// File extends Blob
function File(parts, name, options) {
    Blob.call(this, parts, options);
    this.name = String(name);
    this.lastModified = (options && options.lastModified) || Date.now();
}
File.prototype = Object.create(Blob.prototype);
File.prototype.constructor = File;

globalThis.Blob = Blob;
globalThis.File = File;

})(globalThis);
