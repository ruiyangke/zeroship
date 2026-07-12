// Behavioral JS gate: drive `status` (the journal-read async path) through the
// REAL napi TSFN fire-and-resolve bridge with a canned JS host driver. Proves the
// cross-thread oneshot wakeup reaches the reactor-less block_on THROUGH the real
// N-API boundary under Node (the §B.3 feasibility hinge, empirically), and that the
// Promise resolves with the correct journal outcome.
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const addon = require('../index.js');

const recorded = [];
// napi delivers the (request, done) tuple as a SINGLE array arg.
function hostDriver([request, done]) {
  recorded.push(`${request.kind}: ${request.sql}`);
  // A real event-loop turn so the wakeup is genuinely cross-thread + deferred
  // (mirrors the B.5 spike's setTimeout(…,0) ordering variant).
  setTimeout(() => done(null, { rows: [], rowCount: 1 }), 0);
}

const watchdog = setTimeout(() => {
  console.error('FAIL: async bridge hung (Promise never settled)');
  process.exit(1);
}, 10000);

// Typed verb boundary (redesign step 5a): `status` takes a `StatusRequest` object
// and RESOLVES a typed `StatusReply` — no JSON string, `currentVersion` camelCase.
const status = await addon.status(hostDriver, {
  projectId: 'prj_js',
  projectSchema: 'proj_js',
  migrations: [],
});
clearTimeout(watchdog);

let ok = true;
function check(cond, msg) { if (!cond) { console.error('FAIL:', msg); ok = false; } }

check(recorded.length > 0, 'host driver was never called (TSFN bridge did not fire)');
check(
  recorded.some(s => s.includes('schema_migrations') || s.includes('union_all')),
  'journal read never reached the host driver'
);
check(status.currentVersion === null || status.currentVersion === undefined, `expected null currentVersion on empty journal, got ${status.currentVersion}`);
check(Array.isArray(status.applied) && status.applied.length === 0, 'applied should be empty on empty journal');

if (!ok) { console.error('recorded:', recorded); process.exit(1); }
console.log(`PASS: ${recorded.length} verbs driven through the real napi fire-and-resolve bridge; Promise resolved with the empty-journal outcome`);
console.log('  sample verbs:', recorded.slice(0, 3).join(' | '));
