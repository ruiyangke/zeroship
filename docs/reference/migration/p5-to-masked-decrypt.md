# P5.5 masked-default migration note

Zeroship is still pre-launch. The temporary P5 transparent-decrypt
window never had published creator apps, so there is no supported
creator-codebase migration workflow and no `zeroship migrate
scan-mask-usage` command.

Use the current masking model directly:

- Reads of masked columns return `MaskedValue<T>` by default.
- Plaintext requires an explicit `.unmask(...)` call or query-level
  unmask hint.
- The current API and examples live in [db.md](../db.md).
