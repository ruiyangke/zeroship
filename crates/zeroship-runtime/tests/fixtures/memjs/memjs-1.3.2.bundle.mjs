var __defProp = Object.defineProperty;
var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
var __getOwnPropNames = Object.getOwnPropertyNames;
var __hasOwnProp = Object.prototype.hasOwnProperty;
var __esm = (fn, res) => function __init() {
  return fn && (res = (0, fn[__getOwnPropNames(fn)[0]])(fn = 0)), res;
};
var __commonJS = (cb, mod) => function __require() {
  return mod || (0, cb[__getOwnPropNames(cb)[0]])((mod = { exports: {} }).exports, mod), mod.exports;
};
var __export = (target, all) => {
  for (var name in all)
    __defProp(target, name, { get: all[name], enumerable: true });
};
var __copyProps = (to, from, except, desc) => {
  if (from && typeof from === "object" || typeof from === "function") {
    for (let key of __getOwnPropNames(from))
      if (!__hasOwnProp.call(to, key) && key !== except)
        __defProp(to, key, { get: () => from[key], enumerable: !(desc = __getOwnPropDesc(from, key)) || desc.enumerable });
  }
  return to;
};
var __reExport = (target, mod, secondTarget) => (__copyProps(target, mod, "default"), secondTarget && __copyProps(secondTarget, mod, "default"));
var __toCommonJS = (mod) => __copyProps(__defProp({}, "__esModule", { value: true }), mod);

// node_modules/memjs/lib/memjs/protocol.js
var require_protocol = __commonJS({
  "node_modules/memjs/lib/memjs/protocol.js"(exports) {
    exports.errors = {};
    exports.errors[0] = "No error";
    exports.errors[1] = "Key not found";
    exports.errors[2] = "Key exists";
    exports.errors[3] = "Value too large";
    exports.errors[4] = "Invalid arguments";
    exports.errors[5] = "Item not stored";
    exports.errors[6] = "Incr/Decr on non-numeric value";
    exports.errors[7] = "The vbucket belongs to another server";
    exports.errors[8] = "Authentication error";
    exports.errors[9] = "Authentication continue";
    exports.errors[129] = "Unknown command";
    exports.errors[130] = "Out of memory";
    exports.errors[131] = "Not supported";
    exports.errors[132] = "Internal error";
    exports.errors[133] = "Busy";
    exports.errors[134] = "Temporary failure";
  }
});

// shims/net.js
var net_exports = {};
__export(net_exports, {
  default: () => net_default
});
import * as star from "node:net";
import def from "node:net";
import * as node_net_star from "node:net";
var net_default;
var init_net = __esm({
  "shims/net.js"() {
    __reExport(net_exports, node_net_star);
    net_default = def ?? star;
  }
});

// shims/events.js
var events_exports = {};
__export(events_exports, {
  default: () => events_default
});
import * as star2 from "node:events";
import def2 from "node:events";
import * as node_events_star from "node:events";
var events_default;
var init_events = __esm({
  "shims/events.js"() {
    __reExport(events_exports, node_events_star);
    events_default = def2 ?? star2;
  }
});

// shims/util.js
var util_exports = {};
__export(util_exports, {
  default: () => util_default
});
import * as star3 from "node:util";
import def3 from "node:util";
import * as node_util_star from "node:util";
var util_default;
var init_util = __esm({
  "shims/util.js"() {
    __reExport(util_exports, node_util_star);
    util_default = def3 ?? star3;
  }
});

// node_modules/memjs/lib/memjs/header.js
var require_header = __commonJS({
  "node_modules/memjs/lib/memjs/header.js"(exports) {
    exports.fromBuffer = function(headerBuf) {
      if (!headerBuf) {
        return {};
      }
      return {
        magic: headerBuf.readUInt8(0),
        opcode: headerBuf.readUInt8(1),
        keyLength: headerBuf.readUInt16BE(2),
        extrasLength: headerBuf.readUInt8(4),
        dataType: headerBuf.readUInt8(5),
        status: headerBuf.readUInt16BE(6),
        totalBodyLength: headerBuf.readUInt32BE(8),
        opaque: headerBuf.readUInt32BE(12),
        cas: headerBuf.slice(16, 24)
      };
    };
    exports.toBuffer = function(header) {
      var headerBuf = Buffer.alloc(24);
      headerBuf.fill();
      headerBuf.writeUInt8(header.magic, 0);
      headerBuf.writeUInt8(header.opcode, 1);
      headerBuf.writeUInt16BE(header.keyLength, 2);
      headerBuf.writeUInt8(header.extrasLength, 4);
      headerBuf.writeUInt8(header.dataType || 0, 5);
      headerBuf.writeUInt16BE(header.status || 0, 6);
      headerBuf.writeUInt32BE(header.totalBodyLength, 8);
      headerBuf.writeUInt32BE(header.opaque || 0, 12);
      if (header.cas) {
        header.cas.copy(headerBuf, 16);
      } else {
        headerBuf.fill("\0", 16);
      }
      return headerBuf;
    };
  }
});

// node_modules/memjs/lib/memjs/utils.js
var require_utils = __commonJS({
  "node_modules/memjs/lib/memjs/utils.js"(exports) {
    var header = require_header();
    var bufferify = function(val) {
      return Buffer.isBuffer(val) ? val : Buffer.from(val);
    };
    exports.makeRequestBuffer = function(opcode, key, extras, value, opaque) {
      key = bufferify(key);
      extras = bufferify(extras);
      value = bufferify(value);
      var buf = Buffer.alloc(24 + key.length + extras.length + value.length);
      buf.fill();
      var requestHeader = {
        magic: 128,
        opcode,
        keyLength: key.length,
        extrasLength: extras.length,
        totalBodyLength: key.length + value.length + extras.length,
        opaque
      };
      header.toBuffer(requestHeader).copy(buf);
      extras.copy(buf, 24);
      key.copy(buf, 24 + extras.length);
      value.copy(buf, 24 + extras.length + key.length);
      return buf;
    };
    exports.makeAmountInitialAndExpiration = function(amount, amountIfEmpty, expiration) {
      var buf = Buffer.alloc(20);
      buf.writeUInt32BE(0, 0);
      buf.writeUInt32BE(amount, 4);
      buf.writeUInt32BE(0, 8);
      buf.writeUInt32BE(amountIfEmpty, 12);
      buf.writeUInt32BE(expiration, 16);
      return buf;
    };
    exports.makeExpiration = function(expiration) {
      var buf = Buffer.alloc(4);
      buf.writeUInt32BE(expiration, 0);
      return buf;
    };
    exports.hashCode = function(str) {
      var ret, i, len;
      for (ret = 0, i = 0, len = str.length; i < len; i++) {
        ret = 31 * ret + str.charCodeAt(i) << 0;
      }
      return Math.abs(ret);
    };
    exports.parseMessage = function(dataBuf) {
      if (dataBuf.length < 24) {
        return false;
      }
      var responseHeader = header.fromBuffer(dataBuf);
      if (dataBuf.length < responseHeader.totalBodyLength + 24 || responseHeader.totalBodyLength < responseHeader.keyLength + responseHeader.extrasLength) {
        return false;
      }
      var pointer = 24;
      var extras = dataBuf.slice(pointer, pointer + responseHeader.extrasLength);
      pointer += responseHeader.extrasLength;
      var key = dataBuf.slice(pointer, pointer + responseHeader.keyLength);
      pointer += responseHeader.keyLength;
      var val = dataBuf.slice(pointer, 24 + responseHeader.totalBodyLength);
      return { header: responseHeader, key, extras, val };
    };
    exports.merge = function(original, deflt) {
      var attr, originalValue;
      for (attr in deflt) {
        if (deflt.hasOwnProperty(attr)) {
          originalValue = original[attr];
          if (originalValue === void 0 || originalValue === null) {
            original[attr] = deflt[attr];
          }
        }
      }
      return original;
    };
    exports.timestamp = function() {
      var times = process.hrtime();
      return times[0] * 1e3 + Math.round(times[1] / 1e6);
    };
    if (!Buffer.concat) {
      Buffer.concat = function(list, length) {
        if (!Array.isArray(list)) {
          throw new Error("Usage: Buffer.concat(list, [length])");
        }
        if (list.length === 0) {
          return Buffer.alloc(0);
        }
        if (list.length === 1) {
          return list[0];
        }
        var i, buf;
        if (typeof length !== "number") {
          length = 0;
          for (i = 0; i < list.length; i++) {
            buf = list[i];
            length += buf.length;
          }
        }
        var buffer = Buffer.alloc(length);
        var pos = 0;
        for (i = 0; i < list.length; i++) {
          buf = list[i];
          buf.copy(buffer, pos);
          pos += buf.length;
        }
        return buffer;
      };
    }
  }
});

// node_modules/memjs/lib/memjs/server.js
var require_server = __commonJS({
  "node_modules/memjs/lib/memjs/server.js"(exports) {
    var net = (init_net(), __toCommonJS(net_exports));
    var events = (init_events(), __toCommonJS(events_exports));
    var util = (init_util(), __toCommonJS(util_exports));
    var makeRequestBuffer = require_utils().makeRequestBuffer;
    var parseMessage = require_utils().parseMessage;
    var merge = require_utils().merge;
    var timestamp = require_utils().timestamp;
    var Server = function(host, port, username, password, options) {
      events.EventEmitter.call(this);
      this.responseBuffer = Buffer.from([]);
      this.host = host;
      this.port = port;
      this.connected = false;
      this.timeoutSet = false;
      this.connectCallbacks = [];
      this.responseCallbacks = {};
      this.requestTimeouts = [];
      this.errorCallbacks = {};
      this.options = merge(options || {}, { timeout: 0.5, keepAlive: false, keepAliveDelay: 30 });
      if (this.options.conntimeout === void 0 || this.options.conntimeout === null) {
        this.options.conntimeout = 2 * this.options.timeout;
      }
      this.username = username || this.options.username || process.env.MEMCACHIER_USERNAME || process.env.MEMCACHE_USERNAME;
      this.password = password || this.options.password || process.env.MEMCACHIER_PASSWORD || process.env.MEMCACHE_PASSWORD;
      return this;
    };
    util.inherits(Server, events.EventEmitter);
    Server.prototype.onConnect = function(func) {
      this.connectCallbacks.push(func);
    };
    Server.prototype.onResponse = function(seq, func) {
      this.responseCallbacks[seq] = func;
    };
    Server.prototype.respond = function(response) {
      var callback = this.responseCallbacks[response.header.opaque];
      if (!callback) {
        return;
      }
      callback(response);
      if (!callback.quiet || response.header.totalBodyLength === 0) {
        delete this.responseCallbacks[response.header.opaque];
        this.requestTimeouts.shift();
        delete this.errorCallbacks[response.header.opaque];
      }
    };
    Server.prototype.onError = function(seq, func) {
      this.errorCallbacks[seq] = func;
    };
    Server.prototype.error = function(err) {
      var errcalls = this.errorCallbacks;
      this.connectCallbacks = [];
      this.responseCallbacks = {};
      this.requestTimeouts = [];
      this.errorCallbacks = {};
      this.timeoutSet = false;
      if (this._socket) {
        this._socket.destroy();
        delete this._socket;
      }
      var k;
      for (k in errcalls) {
        if (errcalls.hasOwnProperty(k)) {
          errcalls[k](err);
        }
      }
    };
    Server.prototype.listSasl = function() {
      var buf = makeRequestBuffer(32, "", "", "");
      this.writeSASL(buf);
    };
    Server.prototype.saslAuth = function() {
      var authStr = "\0" + this.username + "\0" + this.password;
      var buf = makeRequestBuffer(33, "PLAIN", "", authStr);
      this.writeSASL(buf);
    };
    Server.prototype.appendToBuffer = function(dataBuf) {
      var old = this.responseBuffer;
      this.responseBuffer = Buffer.alloc(old.length + dataBuf.length);
      old.copy(this.responseBuffer, 0);
      dataBuf.copy(this.responseBuffer, old.length);
      return this.responseBuffer;
    };
    Server.prototype.responseHandler = function(dataBuf) {
      var response = parseMessage(this.appendToBuffer(dataBuf));
      var respLength;
      while (response) {
        if (response.header.opcode === 32) {
          this.saslAuth();
        } else if (response.header.status === 32) {
          this.error("Memcached server authentication failed!");
        } else if (response.header.opcode === 33) {
          this.emit("authenticated");
        } else {
          this.respond(response);
        }
        respLength = response.header.totalBodyLength + 24;
        this.responseBuffer = this.responseBuffer.slice(respLength);
        response = parseMessage(this.responseBuffer);
      }
    };
    Server.prototype.sock = function(sasl, go) {
      var self = this;
      if (!self._socket) {
        self.connected = false;
        self._socket = net.connect(this.port, this.host, function() {
          self.once("authenticated", function() {
            if (self._socket) {
              self.connected = true;
              self._socket.setTimeout(0);
              self.timeoutSet = false;
              go(self._socket);
              self.connectCallbacks.forEach(function(cb) {
                cb(self._socket);
              });
              self.connectCallbacks = [];
            }
          });
          this.on("data", function(dataBuf) {
            self.responseHandler(dataBuf);
          });
          if (self.username && self.password) {
            self.listSasl();
          } else {
            self.emit("authenticated");
          }
        });
        self._socket.on("error", function(error) {
          self.error(error);
        });
        self._socket.on("close", function() {
          self.connected = false;
          if (self.timeoutSet) {
            self._socket.setTimeout(0);
            self.timeoutSet = false;
          }
          self._socket = void 0;
        });
        self.timeoutSet = true;
        self._socket.setTimeout(self.options.conntimeout * 1e3, function() {
          self.timeoutSet = false;
          if (!self.connected) {
            this.end();
            self._socket = void 0;
            self.error(new Error("socket timed out connecting to server."));
          }
        });
        self._socket.setKeepAlive(self.options.keepAlive, self.options.keepAliveDelay * 1e3);
      } else if (!self.connected && !sasl) {
        self.onConnect(go);
      } else {
        go(self._socket);
      }
    };
    var timeoutHandler = function(server, sock) {
      if (server.requestTimeouts.length === 0) {
        server.timeoutSet = false;
        return;
      }
      var now = timestamp();
      var soonestTimeout = server.requestTimeouts[0];
      if (soonestTimeout <= now) {
        sock.end();
        server.connected = false;
        server._socket = void 0;
        server.timeoutSet = false;
        server.error(new Error("socket timed out waiting on response."));
      } else {
        var deadline = soonestTimeout - now;
        sock.setTimeout(deadline, function() {
          timeoutHandler(server, sock);
        });
      }
    };
    Server.prototype.write = function(blob) {
      var self = this;
      var deadline = Math.round(self.options.timeout * 1e3);
      this.sock(false, function(s) {
        s.write(blob);
        self.requestTimeouts.push(timestamp() + deadline);
        if (!self.timeoutSet) {
          self.timeoutSet = true;
          s.setTimeout(deadline, function() {
            timeoutHandler(self, this);
          });
        }
      });
    };
    Server.prototype.writeSASL = function(blob) {
      this.sock(true, function(s) {
        s.write(blob);
      });
    };
    Server.prototype.close = function() {
      if (this._socket) {
        this._socket.end();
      }
    };
    Server.prototype.toString = function() {
      return "<Server " + this.host + ":" + this.port + ">";
    };
    exports.Server = Server;
  }
});

// node_modules/memjs/lib/memjs/noop-serializer.js
var require_noop_serializer = __commonJS({
  "node_modules/memjs/lib/memjs/noop-serializer.js"(exports) {
    var noopSerializer = {
      serialize: function(opcode, value, extras) {
        return { value, extras };
      },
      deserialize: function(opcode, value, extras) {
        return { value, extras };
      }
    };
    exports.noopSerializer = noopSerializer;
  }
});

// node_modules/memjs/lib/memjs/memjs.js
var require_memjs = __commonJS({
  "node_modules/memjs/lib/memjs/memjs.js"(exports) {
    var errors = require_protocol().errors;
    var Server = require_server().Server;
    var noopSerializer = require_noop_serializer().noopSerializer;
    var makeRequestBuffer = require_utils().makeRequestBuffer;
    var hashCode = require_utils().hashCode;
    var merge = require_utils().merge;
    var makeExpiration = require_utils().makeExpiration;
    var makeAmountInitialAndExpiration = require_utils().makeAmountInitialAndExpiration;
    var Client = function(servers, options) {
      this.servers = servers;
      this.seq = 0;
      this.options = merge(
        options || {},
        { failoverTime: 60, retries: 2, retry_delay: 0.2, expires: 0, logger: console }
      );
      this.serializer = this.options.serializer || noopSerializer;
    };
    Client.create = function(serversStr, options) {
      serversStr = serversStr || process.env.MEMCACHIER_SERVERS || process.env.MEMCACHE_SERVERS || "localhost:11211";
      var serverUris = serversStr.split(",");
      var servers = serverUris.map(function(uri) {
        var uriParts = uri.split("@");
        var hostPort = uriParts[uriParts.length - 1].split(":");
        var userPass = (uriParts[uriParts.length - 2] || "").split(":");
        return new Server(hostPort[0], parseInt(hostPort[1] || 11211, 10), userPass[0], userPass[1], options);
      });
      return new Client(servers, options);
    };
    Client.prototype.getServer = function(key) {
      return hashCode(key) % this.servers.length;
    };
    Client.prototype.server = function(key) {
      var total = this.servers.length;
      var origIdx = total > 1 ? this.getServer(key) : 0;
      var idx = origIdx;
      var serv = this.servers[idx];
      while (serv.wakeupAt && serv.wakeupAt > Date.now()) {
        idx = (idx + 1) % total;
        if (idx === origIdx) {
          return null;
        }
        serv = this.servers[idx];
      }
      return serv;
    };
    var promisify = function(command) {
      return new Promise(function(resolve, reject) {
        command(function(err, result) {
          err ? reject(err) : resolve(result);
        });
      });
    };
    Client.prototype.get = function(key, callback) {
      var self = this;
      if (callback === void 0) {
        return promisify(function(callback2) {
          self.get(key, function(err, value, flags) {
            callback2(err, { value, flags });
          });
        });
      }
      var logger = this.options.logger;
      this.incrSeq();
      var request = makeRequestBuffer(0, key, "", "", this.seq);
      this.perform(key, request, this.seq, function(err, response) {
        if (err) {
          if (callback) {
            callback(err, null, null);
          }
          return;
        }
        switch (response.header.status) {
          case 0:
            if (callback) {
              var deserialized = self.serializer.deserialize(response.header.opcode, response.val, response.extras);
              callback(null, deserialized.value, deserialized.extras);
            }
            break;
          case 1:
            if (callback) {
              callback(null, null, null);
            }
            break;
          default:
            var errorMessage = "MemJS GET: " + errors[response.header.status];
            logger.log(errorMessage);
            if (callback) {
              callback(new Error(errorMessage), null, null);
            }
        }
      });
    };
    Client.prototype.set = function(key, value, options, callback) {
      if (callback === void 0 && typeof options !== "function") {
        var self = this;
        if (!options) options = {};
        return promisify(function(callback2) {
          self.set(key, value, options, function(err, success) {
            callback2(err, success);
          });
        });
      }
      var logger = this.options.logger;
      var expires;
      if (typeof options === "function" || typeof callback === "number") {
        logger.log("MemJS SET: using deprecated call - arguments have changed");
        expires = callback;
        callback = options;
        options = {};
      }
      logger = this.options.logger;
      expires = options.expires;
      this.incrSeq();
      var expiration = makeExpiration(expires || this.options.expires);
      var extras = Buffer.concat([Buffer.from("00000000", "hex"), expiration]);
      var opcode = 1;
      var serialized = this.serializer.serialize(opcode, value, extras);
      var request = makeRequestBuffer(opcode, key, serialized.extras, serialized.value, this.seq);
      this.perform(key, request, this.seq, function(err, response) {
        if (err) {
          if (callback) {
            callback(err, null);
          }
          return;
        }
        switch (response.header.status) {
          case 0:
            if (callback) {
              callback(null, true);
            }
            break;
          default:
            var errorMessage = "MemJS SET: " + errors[response.header.status];
            logger.log(errorMessage);
            if (callback) {
              callback(new Error(errorMessage), null, null);
            }
        }
      });
    };
    Client.prototype.add = function(key, value, options, callback) {
      if (callback === void 0 && typeof options !== "function") {
        var self = this;
        if (!options) options = {};
        return promisify(function(callback2) {
          self.add(key, value, options, function(err, success) {
            callback2(err, success);
          });
        });
      }
      var logger = this.options.logger;
      var expires;
      if (typeof options === "function") {
        logger.log("MemJS ADD: using deprecated call - arguments have changed");
        expires = callback;
        callback = options;
        options = {};
      }
      logger = this.options.logger;
      expires = options.expires;
      this.incrSeq();
      var expiration = makeExpiration(expires || this.options.expires);
      var extras = Buffer.concat([Buffer.from("00000000", "hex"), expiration]);
      var opcode = 2;
      var serialized = this.serializer.serialize(opcode, value, extras);
      var request = makeRequestBuffer(opcode, key, serialized.extras, serialized.value, this.seq);
      this.perform(key, request, this.seq, function(err, response) {
        if (err) {
          if (callback) {
            callback(err, null, null);
          }
          return;
        }
        switch (response.header.status) {
          case 0:
            if (callback) {
              callback(null, true);
            }
            break;
          case 2:
            if (callback) {
              callback(null, false);
            }
            break;
          default:
            var errorMessage = "MemJS ADD: " + errors[response.header.status];
            logger.log(errorMessage, false);
            if (callback) {
              callback(new Error(errorMessage), null, null);
            }
        }
      });
    };
    Client.prototype.replace = function(key, value, options, callback) {
      if (callback === void 0 && typeof options !== "function") {
        var self = this;
        if (!options) options = {};
        return promisify(function(callback2) {
          self.replace(key, value, options, function(err, success) {
            callback2(err, success);
          });
        });
      }
      var logger = this.options.logger;
      var expires;
      if (typeof options === "function") {
        logger.log("MemJS REPLACE: using deprecated call - arguments have changed");
        expires = callback;
        callback = options;
        options = {};
      }
      logger = this.options.logger;
      expires = options.expires;
      this.incrSeq();
      var expiration = makeExpiration(expires || this.options.expires);
      var extras = Buffer.concat([Buffer.from("00000000", "hex"), expiration]);
      var opcode = 3;
      var serialized = this.serializer.serialize(opcode, value, extras);
      var request = makeRequestBuffer(opcode, key, serialized.extras, serialized.value, this.seq);
      this.perform(key, request, this.seq, function(err, response) {
        if (err) {
          if (callback) {
            callback(err, null, null);
          }
          return;
        }
        switch (response.header.status) {
          case 0:
            if (callback) {
              callback(null, true);
            }
            break;
          case 1:
            if (callback) {
              callback(null, false);
            }
            break;
          default:
            var errorMessage = "MemJS REPLACE: " + errors[response.header.status];
            logger.log(errorMessage, false);
            if (callback) {
              callback(new Error(errorMessage), null, null);
            }
        }
      });
    };
    Client.prototype.delete = function(key, callback) {
      if (callback === void 0) {
        var self = this;
        return promisify(function(callback2) {
          self.delete(key, function(err, success) {
            callback2(err, success);
          });
        });
      }
      var logger = this.options.logger;
      this.incrSeq();
      var request = makeRequestBuffer(4, key, "", "", this.seq);
      this.perform(key, request, this.seq, function(err, response) {
        if (err) {
          if (callback) {
            callback(err, null, null);
          }
          return;
        }
        switch (response.header.status) {
          case 0:
            if (callback) {
              callback(null, true);
            }
            break;
          case 1:
            if (callback) {
              callback(null, false);
            }
            break;
          default:
            var errorMessage = "MemJS DELETE: " + errors[response.header.status];
            logger.log(errorMessage, false);
            if (callback) {
              callback(new Error(errorMessage), null);
            }
        }
      });
    };
    Client.prototype.increment = function(key, amount, options, callback) {
      if (callback === void 0 && typeof options !== "function") {
        var self = this;
        return promisify(function(callback2) {
          if (!options) options = {};
          self.increment(key, amount, options, function(err, success, value) {
            callback2(err, { success, value });
          });
        });
      }
      var logger = this.options.logger;
      var initial;
      var expires;
      if (typeof options === "function") {
        logger.log("MemJS INCREMENT: using deprecated call - arguments have changed");
        initial = arguments[4];
        expires = callback;
        callback = options;
        options = {};
      }
      logger = this.options.logger;
      initial = options.initial;
      expires = options.expires;
      this.incrSeq();
      initial = initial || 0;
      expires = expires || this.options.expires;
      var extras = makeAmountInitialAndExpiration(amount, initial, expires);
      var request = makeRequestBuffer(5, key, extras, "", this.seq);
      this.perform(key, request, this.seq, function(err, response) {
        if (err) {
          if (callback) {
            callback(err, null);
          }
          return;
        }
        switch (response.header.status) {
          case 0:
            var bufInt = (response.val.readUInt32BE(0) << 8) + response.val.readUInt32BE(4);
            if (callback) {
              callback(null, true, bufInt);
            }
            break;
          default:
            var errorMessage = "MemJS INCREMENT: " + errors[response.header.status];
            logger.log(errorMessage);
            if (callback) {
              callback(new Error(errorMessage), null, null);
            }
        }
      });
    };
    Client.prototype.decrement = function(key, amount, options, callback) {
      if (callback === void 0 && typeof options !== "function") {
        var self = this;
        return promisify(function(callback2) {
          self.decrement(key, amount, options, function(err, success, value) {
            callback2(err, { success, value });
          });
        });
      }
      var logger = this.options.logger;
      var initial;
      var expires;
      if (typeof options === "function") {
        logger.log("MemJS DECREMENT: using deprecated call - arguments have changed");
        initial = arguments[4];
        expires = callback;
        callback = options;
        options = {};
      }
      logger = this.options.logger;
      initial = options.initial;
      expires = options.expires;
      this.incrSeq();
      initial = initial || 0;
      expires = expires || this.options.expires;
      var extras = makeAmountInitialAndExpiration(amount, initial, expires);
      var request = makeRequestBuffer(6, key, extras, "", this.seq);
      this.perform(key, request, this.seq, function(err, response) {
        if (err) {
          if (callback) {
            callback(err, null);
          }
          return;
        }
        switch (response.header.status) {
          case 0:
            var bufInt = (response.val.readUInt32BE(0) << 8) + response.val.readUInt32BE(4);
            if (callback) {
              callback(null, true, bufInt);
            }
            break;
          default:
            var errorMessage = "MemJS DECREMENT: " + errors[response.header.status];
            logger.log(errorMessage);
            if (callback) {
              callback(new Error(errorMessage), null, null);
            }
        }
      });
    };
    Client.prototype.append = function(key, value, callback) {
      if (callback === void 0) {
        var self = this;
        return promisify(function(callback2) {
          self.append(key, value, function(err, success) {
            callback2(err, success);
          });
        });
      }
      var logger = this.options.logger;
      this.incrSeq();
      var opcode = 14;
      var serialized = this.serializer.serialize(opcode, value, "");
      var request = makeRequestBuffer(opcode, key, serialized.extras, serialized.value, this.seq);
      this.perform(key, request, this.seq, function(err, response) {
        if (err) {
          if (callback) {
            callback(err, null);
          }
          return;
        }
        switch (response.header.status) {
          case 0:
            if (callback) {
              callback(null, true);
            }
            break;
          case 1:
            if (callback) {
              callback(null, false);
            }
            break;
          default:
            var errorMessage = "MemJS APPEND: " + errors[response.header.status];
            logger.log(errorMessage);
            if (callback) {
              callback(new Error(errorMessage), null);
            }
        }
      });
    };
    Client.prototype.prepend = function(key, value, callback) {
      if (callback === void 0) {
        var self = this;
        return promisify(function(callback2) {
          self.prepend(key, value, function(err, success) {
            callback2(err, success);
          });
        });
      }
      var logger = this.options.logger;
      this.incrSeq();
      var opcode = 14;
      var serialized = this.serializer.serialize(opcode, value, "");
      var request = makeRequestBuffer(opcode, key, serialized.extras, serialized.value, this.seq);
      this.perform(key, request, this.seq, function(err, response) {
        if (err) {
          if (callback) {
            callback(err, null);
          }
          return;
        }
        switch (response.header.status) {
          case 0:
            if (callback) {
              callback(null, true);
            }
            break;
          case 1:
            if (callback) {
              callback(null, false);
            }
            break;
          default:
            var errorMessage = "MemJS PREPEND: " + errors[response.header.status];
            logger.log(errorMessage);
            if (callback) {
              callback(new Error(errorMessage), null);
            }
        }
      });
    };
    Client.prototype.touch = function(key, expires, callback) {
      if (callback === void 0) {
        var self = this;
        return promisify(function(callback2) {
          self.touch(key, expires, function(err, success) {
            callback2(err, success);
          });
        });
      }
      var logger = this.options.logger;
      this.incrSeq();
      var extras = makeExpiration(expires || this.options.expires);
      var request = makeRequestBuffer(28, key, extras, "", this.seq);
      this.perform(key, request, this.seq, function(err, response) {
        if (err) {
          if (callback) {
            callback(err, null);
          }
          return;
        }
        switch (response.header.status) {
          case 0:
            if (callback) {
              callback(null, true);
            }
            break;
          case 1:
            if (callback) {
              callback(null, false);
            }
            break;
          default:
            var errorMessage = "MemJS TOUCH: " + errors[response.header.status];
            logger.log(errorMessage);
            if (callback) {
              callback(new Error(errorMessage), null);
            }
        }
      });
    };
    Client.prototype.flush = function(callback) {
      if (callback === void 0) {
        var self = this;
        return promisify(function(callback2) {
          self.flush(function(err, results) {
            callback2(err, results);
          });
        });
      }
      this.incrSeq();
      var request = makeRequestBuffer(8, "", "", "", this.seq);
      var count = this.servers.length;
      var result = {};
      var lastErr = null;
      var i;
      var handleFlush = function(seq, serv) {
        serv.onResponse(seq, function() {
          count -= 1;
          result[serv.host + ":" + serv.port] = true;
          if (callback && count === 0) {
            callback(lastErr, result);
          }
        });
        serv.onError(seq, function(err) {
          count -= 1;
          lastErr = err;
          result[serv.host + ":" + serv.port] = err;
          if (callback && count === 0) {
            callback(lastErr, result);
          }
        });
        serv.write(request);
      };
      for (i = 0; i < this.servers.length; i++) {
        handleFlush(this.seq, this.servers[i]);
      }
    };
    Client.prototype.statsWithKey = function(key, callback) {
      var logger = this.options.logger;
      this.incrSeq();
      var request = makeRequestBuffer(16, key, "", "", this.seq);
      var i;
      var handleStats = function(seq, serv) {
        var result = {};
        var handle = function(response) {
          if (response.header.totalBodyLength === 0) {
            if (callback) {
              callback(null, serv.host + ":" + serv.port, result);
            }
            return;
          }
          switch (response.header.status) {
            case 0:
              result[response.key.toString()] = response.val.toString();
              break;
            default:
              var errorMessage = "MemJS STATS (" + key + "): " + errors[response.header.status];
              logger.log(errorMessage, false);
              if (callback) {
                callback(new Error(errorMessage), serv.host + ":" + serv.port, null);
              }
          }
        };
        handle.quiet = true;
        serv.onResponse(seq, handle);
        serv.onError(seq, function(err) {
          if (callback) {
            callback(err, serv.host + ":" + serv.port, null);
          }
        });
        serv.write(request);
      };
      for (i = 0; i < this.servers.length; i++) {
        handleStats(this.seq, this.servers[i]);
      }
    };
    Client.prototype.stats = function(callback) {
      this.statsWithKey("", callback);
    };
    Client.prototype.resetStats = function(callback) {
      this.statsWithKey("reset", callback);
    };
    Client.prototype.quit = function() {
      this.incrSeq();
      var request = makeRequestBuffer(7, "", "", "", this.seq);
      var serv;
      var i;
      var handleQuit = function(seq, serv2) {
        serv2.onResponse(seq, function() {
          serv2.close();
        });
        serv2.onError(seq, function() {
          serv2.close();
        });
        serv2.write(request);
      };
      for (i = 0; i < this.servers.length; i++) {
        serv = this.servers[i];
        handleQuit(this.seq, serv);
      }
    };
    Client.prototype.close = function() {
      var i;
      for (i = 0; i < this.servers.length; i++) {
        this.servers[i].close();
      }
    };
    Client.prototype.perform = function(key, request, seq, callback, retries) {
      var _this = this;
      var serv = this.server(key);
      if (!serv) {
        if (callback) {
          callback(new Error("No servers available"), null);
        }
        return;
      }
      retries = retries || this.options.retries;
      var failover = this.options.failover;
      var failoverTime = this.options.failoverTime;
      var origRetries = this.options.retries;
      var logger = this.options.logger;
      var retry_delay = this.options.retry_delay;
      var responseHandler = function(response) {
        if (callback) {
          callback(null, response);
        }
      };
      var errorHandler = function(error) {
        if (--retries > 0) {
          setTimeout(function() {
            _this.perform(key, request, seq, callback, retries);
          }, 1e3 * retry_delay);
        } else {
          logger.log("MemJS: Server <" + serv.host + ":" + serv.port + "> failed after (" + origRetries + ") retries with error - " + error.message);
          if (failover) {
            serv.wakeupAt = Date.now() + failoverTime * 1e3;
            _this.perform(key, request, seq, callback, origRetries);
          } else {
            if (callback) {
              callback(error, null);
            }
          }
        }
      };
      serv.onResponse(seq, responseHandler);
      serv.onError(seq, errorHandler);
      serv.write(request);
    };
    Client.prototype.incrSeq = function() {
      this.seq++;
      this.seq &= 4294967295;
    };
    exports.Client = Client;
    exports.Server = Server;
    exports.Utils = require_utils();
    exports.Header = require_header();
  }
});
export default require_memjs();
