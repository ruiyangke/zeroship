// node:path — POSIX implementation. Linux V8 runtime, no Win32 semantics.
//
// Algorithm shape mirrors Deno std/path/posix and Node's lib/path.js;
// fresh implementation against the Node 22 spec
// (https://nodejs.org/api/path.html). win32 is a stub-throw — see
// `crates/runtime/src/node/path/mod.rs` for the rationale.
(function() {
  const CHAR_FORWARD_SLASH = 47; // '/'
  const CHAR_DOT = 46;           // '.'

  function assertPath(p) {
    if (typeof p !== "string") {
      const e = new TypeError(
        'Path must be a string. Received ' + JSON.stringify(p)
      );
      e.code = "ERR_INVALID_ARG_TYPE";
      throw e;
    }
  }

  // Resolves `.` and `..` segments. `allowAboveRoot` is `false` for
  // absolute paths (where ascending past `/` collapses to `/`) and
  // `true` for relative paths (where leading `..` survives).
  function normalizeString(path, allowAboveRoot, separator) {
    let res = "";
    let lastSegmentLength = 0;
    let lastSlash = -1;
    let dots = 0;
    let code;
    for (let i = 0; i <= path.length; ++i) {
      if (i < path.length) code = path.charCodeAt(i);
      else if (code === CHAR_FORWARD_SLASH) break;
      else code = CHAR_FORWARD_SLASH;
      if (code === CHAR_FORWARD_SLASH) {
        if (lastSlash === i - 1 || dots === 1) {
          // empty or '.'
        } else if (lastSlash !== i - 1 && dots === 2) {
          if (
            res.length < 2 ||
            lastSegmentLength !== 2 ||
            res.charCodeAt(res.length - 1) !== CHAR_DOT ||
            res.charCodeAt(res.length - 2) !== CHAR_DOT
          ) {
            if (res.length > 2) {
              const lastSlashIndex = res.lastIndexOf(separator);
              if (lastSlashIndex === -1) {
                res = "";
                lastSegmentLength = 0;
              } else {
                res = res.slice(0, lastSlashIndex);
                lastSegmentLength = res.length - 1 - res.lastIndexOf(separator);
              }
              lastSlash = i;
              dots = 0;
              continue;
            } else if (res.length !== 0) {
              res = "";
              lastSegmentLength = 0;
              lastSlash = i;
              dots = 0;
              continue;
            }
          }
          if (allowAboveRoot) {
            res += res.length > 0 ? `${separator}..` : "..";
            lastSegmentLength = 2;
          }
        } else {
          if (res.length > 0) {
            res += `${separator}${path.slice(lastSlash + 1, i)}`;
          } else {
            res = path.slice(lastSlash + 1, i);
          }
          lastSegmentLength = i - lastSlash - 1;
        }
        lastSlash = i;
        dots = 0;
      } else if (code === CHAR_DOT && dots !== -1) {
        ++dots;
      } else {
        dots = -1;
      }
    }
    return res;
  }

  function resolve(...args) {
    let resolvedPath = "";
    let resolvedAbsolute = false;
    for (let i = args.length - 1; i >= -1 && !resolvedAbsolute; i--) {
      // V8 has no real cwd — base on "/" when we run out of args.
      const path = i >= 0 ? args[i] : "/";
      assertPath(path);
      if (path.length === 0) continue;
      resolvedPath = `${path}/${resolvedPath}`;
      resolvedAbsolute = path.charCodeAt(0) === CHAR_FORWARD_SLASH;
    }
    resolvedPath = normalizeString(resolvedPath, !resolvedAbsolute, "/");
    if (resolvedAbsolute) return `/${resolvedPath}`;
    return resolvedPath.length > 0 ? resolvedPath : ".";
  }

  function normalize(path) {
    assertPath(path);
    if (path.length === 0) return ".";
    const isAbs = path.charCodeAt(0) === CHAR_FORWARD_SLASH;
    const trailingSep = path.charCodeAt(path.length - 1) === CHAR_FORWARD_SLASH;
    path = normalizeString(path, !isAbs, "/");
    if (path.length === 0) {
      if (isAbs) return "/";
      return trailingSep ? "./" : ".";
    }
    if (trailingSep) path += "/";
    return isAbs ? `/${path}` : path;
  }

  function isAbsolute(path) {
    assertPath(path);
    return path.length > 0 && path.charCodeAt(0) === CHAR_FORWARD_SLASH;
  }

  function join(...args) {
    if (args.length === 0) return ".";
    let joined;
    for (let i = 0; i < args.length; ++i) {
      const arg = args[i];
      assertPath(arg);
      if (arg.length > 0) {
        if (joined === undefined) joined = arg;
        else joined += `/${arg}`;
      }
    }
    if (joined === undefined) return ".";
    return normalize(joined);
  }

  function relative(from, to) {
    assertPath(from);
    assertPath(to);
    if (from === to) return "";
    from = resolve(from);
    to = resolve(to);
    if (from === to) return "";

    // Trim leading '/' and trailing '/'.
    let fromStart = 1;
    const fromEnd = from.length;
    const fromLen = fromEnd - fromStart;
    let toStart = 1;
    const toLen = to.length - toStart;

    const length = fromLen < toLen ? fromLen : toLen;
    let lastCommonSep = -1;
    let i = 0;
    for (; i < length; i++) {
      const fromCode = from.charCodeAt(fromStart + i);
      if (fromCode !== to.charCodeAt(toStart + i)) break;
      else if (fromCode === CHAR_FORWARD_SLASH) lastCommonSep = i;
    }
    if (i === length) {
      if (toLen > length) {
        if (to.charCodeAt(toStart + i) === CHAR_FORWARD_SLASH) {
          return to.slice(toStart + i + 1);
        }
        if (i === 0) return to.slice(toStart + i);
      } else if (fromLen > length) {
        if (from.charCodeAt(fromStart + i) === CHAR_FORWARD_SLASH) {
          lastCommonSep = i;
        } else if (i === 0) {
          lastCommonSep = 0;
        }
      }
    }

    let out = "";
    for (i = fromStart + lastCommonSep + 1; i <= fromEnd; ++i) {
      if (i === fromEnd || from.charCodeAt(i) === CHAR_FORWARD_SLASH) {
        out += out.length === 0 ? ".." : "/..";
      }
    }
    return `${out}${to.slice(toStart + lastCommonSep)}`;
  }

  function dirname(path) {
    assertPath(path);
    if (path.length === 0) return ".";
    const hasRoot = path.charCodeAt(0) === CHAR_FORWARD_SLASH;
    let end = -1;
    let matchedSlash = true;
    for (let i = path.length - 1; i >= 1; --i) {
      if (path.charCodeAt(i) === CHAR_FORWARD_SLASH) {
        if (!matchedSlash) {
          end = i;
          break;
        }
      } else {
        matchedSlash = false;
      }
    }
    if (end === -1) return hasRoot ? "/" : ".";
    if (hasRoot && end === 1) return "//";
    return path.slice(0, end);
  }

  function basename(path, ext) {
    if (ext !== undefined && typeof ext !== "string") {
      const e = new TypeError('"ext" argument must be a string');
      e.code = "ERR_INVALID_ARG_TYPE";
      throw e;
    }
    assertPath(path);
    let start = 0;
    let end = -1;
    let matchedSlash = true;
    let i;
    if (ext !== undefined && ext.length > 0 && ext.length <= path.length) {
      if (ext.length === path.length && ext === path) return "";
      let extIdx = ext.length - 1;
      let firstNonSlashEnd = -1;
      for (i = path.length - 1; i >= 0; --i) {
        const code = path.charCodeAt(i);
        if (code === CHAR_FORWARD_SLASH) {
          if (!matchedSlash) {
            start = i + 1;
            break;
          }
        } else {
          if (firstNonSlashEnd === -1) {
            matchedSlash = false;
            firstNonSlashEnd = i + 1;
          }
          if (extIdx >= 0) {
            if (code === ext.charCodeAt(extIdx)) {
              if (--extIdx === -1) end = i;
            } else {
              extIdx = -1;
              end = firstNonSlashEnd;
            }
          }
        }
      }
      if (start === end) end = firstNonSlashEnd;
      else if (end === -1) end = path.length;
      return path.slice(start, end);
    }
    for (i = path.length - 1; i >= 0; --i) {
      if (path.charCodeAt(i) === CHAR_FORWARD_SLASH) {
        if (!matchedSlash) {
          start = i + 1;
          break;
        }
      } else if (end === -1) {
        matchedSlash = false;
        end = i + 1;
      }
    }
    if (end === -1) return "";
    return path.slice(start, end);
  }

  function extname(path) {
    assertPath(path);
    let startDot = -1;
    let startPart = 0;
    let end = -1;
    let matchedSlash = true;
    let preDotState = 0;
    for (let i = path.length - 1; i >= 0; --i) {
      const code = path.charCodeAt(i);
      if (code === CHAR_FORWARD_SLASH) {
        if (!matchedSlash) {
          startPart = i + 1;
          break;
        }
        continue;
      }
      if (end === -1) {
        matchedSlash = false;
        end = i + 1;
      }
      if (code === CHAR_DOT) {
        if (startDot === -1) startDot = i;
        else if (preDotState !== 1) preDotState = 1;
      } else if (startDot !== -1) {
        preDotState = -1;
      }
    }
    if (
      startDot === -1 ||
      end === -1 ||
      preDotState === 0 ||
      (preDotState === 1 && startDot === end - 1 && startDot === startPart + 1)
    ) {
      return "";
    }
    return path.slice(startDot, end);
  }

  function format(pathObject) {
    if (pathObject === null || typeof pathObject !== "object") {
      const e = new TypeError(
        'The "pathObject" argument must be of type Object. Received ' +
          (pathObject === null ? "null" : typeof pathObject)
      );
      e.code = "ERR_INVALID_ARG_TYPE";
      throw e;
    }
    const sep = "/";
    const dir = pathObject.dir || pathObject.root;
    const base =
      pathObject.base || `${pathObject.name || ""}${pathObject.ext || ""}`;
    if (!dir) return base;
    return dir === pathObject.root ? `${dir}${base}` : `${dir}${sep}${base}`;
  }

  function parse(path) {
    assertPath(path);
    const ret = { root: "", dir: "", base: "", ext: "", name: "" };
    if (path.length === 0) return ret;
    const isAbs = path.charCodeAt(0) === CHAR_FORWARD_SLASH;
    let start;
    if (isAbs) {
      ret.root = "/";
      start = 1;
    } else {
      start = 0;
    }
    let startDot = -1;
    let startPart = 0;
    let end = -1;
    let matchedSlash = true;
    let i = path.length - 1;
    let preDotState = 0;
    for (; i >= start; --i) {
      const code = path.charCodeAt(i);
      if (code === CHAR_FORWARD_SLASH) {
        if (!matchedSlash) {
          startPart = i + 1;
          break;
        }
        continue;
      }
      if (end === -1) {
        matchedSlash = false;
        end = i + 1;
      }
      if (code === CHAR_DOT) {
        if (startDot === -1) startDot = i;
        else if (preDotState !== 1) preDotState = 1;
      } else if (startDot !== -1) {
        preDotState = -1;
      }
    }
    if (
      startDot === -1 ||
      end === -1 ||
      preDotState === 0 ||
      (preDotState === 1 && startDot === end - 1 && startDot === startPart + 1)
    ) {
      if (end !== -1) {
        if (startPart === 0 && isAbs) ret.base = ret.name = path.slice(1, end);
        else ret.base = ret.name = path.slice(startPart, end);
      }
    } else {
      if (startPart === 0 && isAbs) {
        ret.name = path.slice(1, startDot);
        ret.base = path.slice(1, end);
      } else {
        ret.name = path.slice(startPart, startDot);
        ret.base = path.slice(startPart, end);
      }
      ret.ext = path.slice(startDot, end);
    }
    if (startPart > 0) ret.dir = path.slice(0, startPart - 1);
    else if (isAbs) ret.dir = "/";
    return ret;
  }

  // win32 — stubbed. Linux-only runtime; calling any method throws.
  const win32 = new Proxy(
    {},
    {
      get(_t, prop) {
        if (prop === Symbol.toPrimitive) return () => "[win32 path stub]";
        if (prop === "sep") return "\\";
        if (prop === "delimiter") return ";";
        return function () {
          const e = new Error(
            "node:path win32 is not implemented in zeroship's Linux V8 runtime; use the POSIX (default) path API."
          );
          e.code = "ERR_METHOD_NOT_IMPLEMENTED";
          throw e;
        };
      },
    }
  );

  const path = {
    sep: "/",
    delimiter: ":",
    resolve,
    normalize,
    isAbsolute,
    join,
    relative,
    dirname,
    basename,
    extname,
    format,
    parse,
    win32,
  };
  // Self-reference — `path.posix === path`. Property installed after
  // the object exists so the closure captures the final shape.
  path.posix = path;
  return path;
})()
