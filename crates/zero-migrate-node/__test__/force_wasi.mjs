import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const CHILD_ARGUMENT = '--force-wasi-child';
const NATIVE_REQUEST = 'zero-migrate-loader-test-native';
const NATIVE_AVAILABILITY = 'ZERO_MIGRATE_TEST_NATIVE_AVAILABILITY';
const WASI_AVAILABILITY = 'ZERO_MIGRATE_TEST_WASI_AVAILABILITY';
const WASI_REQUESTS = new Set([
  './zero-migrate-node.wasi.cjs',
  'zero-migrate-node-wasm32-wasi',
]);

function assert(condition, message) {
  if (!condition) {
    console.error(`FAIL: ${message}`);
    process.exit(1);
  }
}

function missingModule(request) {
  const error = new Error(`Cannot find module '${request}'`);
  error.code = 'MODULE_NOT_FOUND';
  return error;
}

function runChild() {
  const require = createRequire(import.meta.url);
  const Module = require('node:module');
  const originalLoad = Module._load;
  let wasiRequests = 0;

  // Replace only loader dependencies so selection is independent of platform artifacts.
  Module._load = function loadTestBinding(request, parent, isMain) {
    if (request === NATIVE_REQUEST) {
      if (process.env[NATIVE_AVAILABILITY] === 'missing') {
        throw missingModule(request);
      }
      return { loaderTestBinding: 'native' };
    }
    if (WASI_REQUESTS.has(request)) {
      wasiRequests += 1;
      if (process.env[WASI_AVAILABILITY] === 'missing') {
        throw missingModule(request);
      }
      return { loaderTestBinding: 'wasi' };
    }
    return originalLoad.call(this, request, parent, isMain);
  };

  try {
    const addon = require('../index.js');
    console.log(JSON.stringify({
      selected: addon.loaderTestBinding,
      wasiRequests,
    }));
  } finally {
    Module._load = originalLoad;
  }
}

function childEnvironment(
  forceWasi,
  nativeAvailability = 'available',
  wasiAvailability = 'available',
) {
  const env = {
    ...process.env,
    NAPI_RS_NATIVE_LIBRARY_PATH: NATIVE_REQUEST,
    [NATIVE_AVAILABILITY]: nativeAvailability,
    [WASI_AVAILABILITY]: wasiAvailability,
  };
  if (forceWasi === undefined) {
    delete env.NAPI_RS_FORCE_WASI;
  } else {
    env.NAPI_RS_FORCE_WASI = forceWasi;
  }
  return env;
}

function spawnCase(forceWasi, nativeAvailability, wasiAvailability) {
  return spawnSync(
    process.execPath,
    [fileURLToPath(import.meta.url), CHILD_ARGUMENT],
    {
      encoding: 'utf8',
      env: childEnvironment(forceWasi, nativeAvailability, wasiAvailability),
    },
  );
}

function resultDetails(result) {
  return [
    `status: ${result.status}`,
    `signal: ${result.signal}`,
    `stdout: ${JSON.stringify(result.stdout)}`,
    `stderr: ${JSON.stringify(result.stderr)}`,
  ].join('\n');
}

function runParent() {
  // A fresh process per value prevents the require cache from hiding selection changes.
  const cases = [
    { label: 'unset', value: undefined, selected: 'native' },
    { label: "'false'", value: 'false', selected: 'native' },
    { label: "'0'", value: '0', selected: 'native' },
    { label: 'another value', value: 'another', selected: 'native' },
    { label: "'true'", value: 'true', selected: 'wasi' },
    { label: "'error'", value: 'error', selected: 'wasi' },
    {
      label: 'unset without native',
      value: undefined,
      nativeAvailability: 'missing',
      selected: 'wasi',
    },
    {
      label: "'true' without WASI",
      value: 'true',
      wasiAvailability: 'missing',
      selected: 'native',
    },
  ];

  for (const testCase of cases) {
    const result = spawnCase(
      testCase.value,
      testCase.nativeAvailability,
      testCase.wasiAvailability,
    );
    assert(
      result.status === 0,
      `${testCase.label} child must exit 0\n${resultDetails(result)}`,
    );
    let outcome;
    try {
      outcome = JSON.parse(result.stdout);
    } catch {
      assert(false, `${testCase.label} child must report selection\n${resultDetails(result)}`);
    }
    assert(
      outcome.selected === testCase.selected,
      `${testCase.label} must select ${testCase.selected}\n${resultDetails(result)}`,
    );
  }

  const requiredWasi = spawnCase('error', 'available', 'missing');
  const requiredWasiMessage =
    'WASI binding not found and NAPI_RS_FORCE_WASI is set to error';
  assert(
    requiredWasi.status === 1,
    `'error' without WASI must exit 1\n${resultDetails(requiredWasi)}`,
  );
  assert(
    requiredWasi.stderr.includes(requiredWasiMessage),
    `'error' without WASI must report the loader error\n${resultDetails(requiredWasi)}`,
  );

  console.log('PASS: NAPI_RS_FORCE_WASI selects native, preferred WASI, and required WASI bindings by exact value');
}

if (process.argv[2] === CHILD_ARGUMENT) {
  runChild();
} else {
  runParent();
}
