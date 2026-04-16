// node:assert polyfill.

function assert(value: any, message?: string): asserts value {
  if (!value) throw new Error(message ?? "Assertion failed");
}

assert.ok = assert;
assert.equal = (a: any, b: any, msg?: string) => { if (a != b) throw new Error(msg ?? `${a} != ${b}`); };
assert.strictEqual = (a: any, b: any, msg?: string) => { if (a !== b) throw new Error(msg ?? `${a} !== ${b}`); };
assert.notEqual = (a: any, b: any, msg?: string) => { if (a == b) throw new Error(msg ?? `${a} == ${b}`); };
assert.notStrictEqual = (a: any, b: any, msg?: string) => { if (a === b) throw new Error(msg ?? `${a} === ${b}`); };
assert.deepEqual = (a: any, b: any, msg?: string) => {
  if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(msg ?? "deepEqual failed");
};
assert.deepStrictEqual = assert.deepEqual;
assert.notDeepEqual = (a: any, b: any, msg?: string) => {
  if (JSON.stringify(a) === JSON.stringify(b)) throw new Error(msg ?? "notDeepEqual failed");
};
assert.notDeepStrictEqual = assert.notDeepEqual;
assert.throws = (fn: () => void, _expected?: any, msg?: string) => {
  try { fn(); throw new Error(msg ?? "Expected function to throw"); }
  catch (e: any) { if (e.message === (msg ?? "Expected function to throw")) throw e; }
};
assert.doesNotThrow = (fn: () => void, _msg?: string) => { fn(); };
assert.rejects = async (fn: () => Promise<any>, _expected?: any, msg?: string) => {
  try { await fn(); throw new Error(msg ?? "Expected promise to reject"); }
  catch (e: any) { if (e.message === (msg ?? "Expected promise to reject")) throw e; }
};
assert.doesNotReject = async (fn: () => Promise<any>) => { await fn(); };
assert.fail = (msg?: string) => { throw new Error(msg ?? "Assert.fail()"); };
assert.ifError = (err: any) => { if (err) throw err; };
assert.match = (str: string, re: RegExp, msg?: string) => {
  if (!re.test(str)) throw new Error(msg ?? `${str} does not match ${re}`);
};

export { assert as strict };
export default assert;
