// `buildInfo()` through the real N-API boundary: the build identity a host logs
// after resolving this `.node` by path. Pins the field shape, the agreement with
// `irVersion()`, and that the identity is constant per loaded addon.
//
// This gate does NOT check that the digest matches any particular tree - only the
// build that produced the addon can know that. It checks the contract a consumer
// depends on: the fields exist, are typed, and do not move between calls.
import { createRequire } from 'module';

const require = createRequire(import.meta.url);
const addon = require('../index.js');

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

const info = addon.buildInfo();

assert(typeof info === 'object' && info !== null,
  `buildInfo() must return an object: ${JSON.stringify(info)}`);
assert(typeof info.version === 'string' && info.version.length > 0,
  `buildInfo().version must be a non-empty string: ${JSON.stringify(info)}`);
assert(Number.isInteger(info.irVersion),
  `buildInfo().irVersion must be an integer: ${JSON.stringify(info)}`);
assert(info.irVersion === addon.irVersion(),
  `buildInfo().irVersion (${info.irVersion}) must equal irVersion() (${addon.irVersion()})`);
assert(/^[0-9a-f]{64}$/.test(info.sourceDigest),
  `buildInfo().sourceDigest must be a lowercase sha256 hex: ${JSON.stringify(info)}`);

// A build identity that changed per call would make any generated-artifact drift
// gate permanently red, so hold it to being constant.
const again = addon.buildInfo();
assert(again.version === info.version
  && again.irVersion === info.irVersion
  && again.sourceDigest === info.sourceDigest,
  `buildInfo() must be constant per loaded addon: ${JSON.stringify(info)} vs ${JSON.stringify(again)}`);

console.log(`PASS: buildInfo reports ${info.version} ir=${info.irVersion} src=${info.sourceDigest.slice(0, 12)}`);
