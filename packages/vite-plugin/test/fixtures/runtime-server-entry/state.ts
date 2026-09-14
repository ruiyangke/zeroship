export const state = { loaded: 0, invoked: 0, loaderKind: "" };

export const input = Object.freeze({
  parse(value: unknown) {
    if (typeof value !== "number") {
      throw Object.assign(new Error("number required"), {
        issues: [{ path: [], message: "Expected a number", code: "invalid_type" }],
      });
    }
    return value * 2;
  },
});
