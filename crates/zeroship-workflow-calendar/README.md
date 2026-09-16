# zeroship-workflow-calendar

Pure schedule definitions and calendar calculation shared by workflow hosts.
Callers supply observation time and the deployment anchor. This crate owns no
scheduling loop, database, network client or executor.

`Calendar` parses the supported cron grammar and IANA timezones. `ScheduleTiming`
also supports intervals anchored to the epoch or deployment activation. Ambiguous
local times choose their earlier instant; nonexistent local times advance to the
next valid local time. `interpretation()` identifies the calculation semantics
and bundled timezone data for persisted manager schedules.

Run `cargo test -p zeroship-workflow-calendar` for grammar, timezone, interval and
metadata contracts. The workflow suite also verifies its dependency boundary.
