# Derive libpq parameter semantics by probing psql

Covers the two surfaces that take connection parameters as text: the
`pg_service.conf` file, and the conninfo string / URI. Their rules are NOT the
same - whitespace around `=` is a syntax error in a service file and perfectly
legal in a conninfo string - so measure the one you are changing.

## Why

`pg_service.conf` has a format DESCRIPTION but not a specification, and the
description does not mention any of the rules that actually matter. This crate's
parser was written from the description and diverged from libpq in five ways,
every one of which would have silently connected somewhere the operator did not
ask for - or refused a file `psql` accepts. Measured 2026-08-25 through the
container below, which means **libpq 18.4** - see the version warning in
Prerequisites before you attribute any reading here to a version.

Re-run this when touching `libs/compio-postgres/src/service.rs`, or when moving
to a libpq whose behaviour might have shifted. Do not carry the table below
forward as fact: re-derive it.

## Prerequisites

Any container with `psql`. The suite's own server has one, and no database is
needed - every probe below fails to RESOLVE a host name, and the host it names
is the answer.

```bash
C=zs-cpg-review-5455      # any postgres:16 container
docker exec $C psql --version
```

**`psql --version` IS NOT THE VERSION YOU ARE MEASURING.** Everything on this
page is CLIENT behaviour, so the oracle is the libpq library; `psql` is only the
program that calls it, and the two carry separate versions. In this very
container they disagree - measured 2026-08-26:

```console
$ docker exec zs-cpg-review-5455 psql --version
psql (PostgreSQL) 16.14 (Debian 16.14-1.pgdg13+1)
$ docker exec zs-cpg-review-5455 dpkg -l | grep -i libpq
ii  libpq5:amd64  18.4-1.pgdg13+1  amd64  PostgreSQL C client library
$ docker exec zs-cpg-review-5455 sh -c 'ls -l /usr/lib/*/libpq.so.5'
... libpq.so.5 -> libpq.so.5.18
```

The image tag is `postgres:16` and the SERVER really is 16.14, which is why the
mismatch reads as settled when it is not. This is not hypothetical: it put a
wrong comment in `config.rs` asserting `connect_timeout=1` "is honoured as one
second, on libpq 16.15 AND 18.4" and concluding there is no floor to match.
PostgreSQL 16 does hold the floor - `if (timeout < 2) timeout = 2;` in
`connectDBComplete`, `fe-connect.c:2439` on `REL_16_STABLE` - and 18 removed it.
Both readings had been taken through 18.4.

So run BOTH lines and quote the library, never the caller:

```bash
docker exec $C dpkg -l | grep -i libpq       # the version that matters
docker exec $C sh -c 'ls -l /usr/lib/*/libpq.so.5'  # the soname it loads
```

To measure a SPECIFIC libpq, use a container whose `libpq5` is that version and
prove it with those two commands. Do not infer it from the image tag, from
`psql --version`, or from the server's `SELECT version()` - none of the three
constrains the client library.

## The instrument

Point every parameter at a `.invalid` host. libpq then reports

```
could not translate host name "<value>" to address: Name or service not known
```

and the quoted value is *exactly* what the parser produced, whitespace
included. That makes the error message a read-out of the parse, which is why
this works without a server, without auth, and without ambiguity.

The message does NOT trim what it prints - proven by the `aftereq` row below,
where a leading space survives into the output. That single row is the control
for every whitespace claim here: without it, "trailing whitespace is stripped"
could just as easily be psql tidying its own message.

```bash
docker exec $C sh -c 'cat > /tmp/svc.conf <<EOF
[lead]
   host=lead.invalid
[beforeeq]
host =beforeeq.invalid
[aftereq]
host= aftereq.invalid
[trail]
host=trail.invalid   
[inline]
host=inline.invalid # a note
[indentcomment]
   # indented comment
host=indentcomment.invalid
EOF
for s in lead beforeeq aftereq trail inline indentcomment; do
  printf "%-16s " "$s"
  PGSERVICEFILE=/tmp/svc.conf psql "service=$s" -c "select 1" 2>&1 | head -1
done'
```

Vary ONE thing per section. The first pass at this used a single section with
several oddities at once and reported `syntax error ... line 10`, which says
that something on that line is wrong and nothing about which.

## What libpq does

| Input | Result |
| --- | --- |
| `   host=x` | accepted; leading whitespace is not part of the key |
| `host =x` | **syntax error** |
| `host= x` | accepted; value is `" x"` - the space is KEPT |
| `host=x   ` | accepted; value is `"x"` - trailing whitespace stripped |
| `host=x # note` | value is `"x # note"`; there are NO inline comments |
| `   # note` | a comment |
| `host=` | accepted; empty value, so the parameter falls back to its default |
| `[name] junk` | section `name`; anything after the `]` is ignored |
| `[two]b]` | section `two` - the name ends at the FIRST `]` |
| `[unclosed` | not a header at all; opens no section |
| `[ prod ]` | section `" prod "`, NOT `prod`; the name is verbatim |
| two sections named `x` | the FIRST one wins |
| one key given twice | the FIRST value wins |

The last two are the ones that connect somewhere wrong rather than failing
loudly, and the duplicate-key rule is the opposite of what "apply each pair in
turn" produces.

```bash
docker exec $C sh -c 'cat > /tmp/svc2.conf <<EOF
[dup]
host=first.invalid
host=second.invalid
EOF
PGSERVICEFILE=/tmp/svc2.conf psql "service=dup" -c "select 1" 2>&1 | head -1'
# -> could not translate host name "first.invalid"
```

## Reading a result

- A `syntax error in service file ... line N` is libpq REFUSING the file. This
  crate must refuse it too; being more permissive means accepting a service that
  `psql` rejects, so the same file works here and fails there.
- `definition of service "x" not found` means the section was never entered -
  the header did not parse the way you assumed.
- A resolved-host error is a successful parse. Read the quoted value.
- If a probe reports something OTHER than a host-resolution failure, it reached
  a real server and the value under test is not what you are reading. Check
  that the `.invalid` host actually landed.

## The other surface: conninfo strings

The same read-out works on a conninfo string, since psql takes one directly:

```bash
docker exec $C sh -c 'psql "host=  extra.invalid" -c "select 1" 2>&1 | head -1'
```

Measured 2026-08-25. Where the service file and the conninfo string disagree,
both columns are given, because the temptation is to assume one parser.

| Input | conninfo | service file |
| --- | --- | --- |
| space around `=` | accepted, stripped | **syntax error** |
| space after `=` | stripped | KEPT in the value |
| `'quoted value'` | quoted, may contain spaces | no quoting; quotes are literal |
| `\` before a character | escape; removed, next char literal | no escaping |
| unterminated `'` | error | n/a |

The conninfo rules above are ALREADY correct in this crate - all fourteen forms
were checked and matched. What was not correct is the empty value.

**`key=` is per-OPTION, and this is the trap.** An enum REFUSES it. `port=`
selects the compiled default. The SIX socket integers reject it. Identity
strings fall back to their defaults rather than keeping an empty value.

**THE `.invalid` INSTRUMENT CANNOT MEASURE THIS, and that is how the row above
used to read "a numeric option takes empty as not given".** Host resolution runs
BEFORE integer validation, so an unresolvable host short-circuits the very check
you are trying to observe, and every integer option prints a resolution failure
that looks like acceptance. Point it at a REACHABLE server for these. Measured
2026-08-26, one variable changed:

```console
$ psql "host=x.invalid connect_timeout=" ...
could not translate host name "x.invalid" to address: Name or service not known
$ psql "host=127.0.0.1 port=5432 user=postgres dbname=zeroship connect_timeout=" ...
... failed: invalid integer value "" for connection option "connect_timeout"
```

`keepalives_idle=` and `tcp_user_timeout=` behave identically, and the source
agrees: `pqParseIntParam` fails when `value == end`, which an empty string
always satisfies (`fe-connect.c`). `port=` is the exception - `DEF_PGPORT_STR`
supplies the compiled default. So run the enums against `.invalid` and the
integers against a live server:

```bash
# enums: the .invalid instrument is fine, nothing reaches the network
docker exec $C sh -c 'for k in sslmode channel_binding target_session_attrs \
    gssencmode load_balance_hosts; do
  printf "%-24s " "$k"
  psql "host=x.invalid $k=" -c "select 1" 2>&1 | head -1 | sed "s/^psql: error: //"
done'

# integers and identities: MUST reach a real server or the check is skipped
docker exec $C sh -c 'for k in port connect_timeout keepalives_idle \
    tcp_user_timeout keepalives keepalives_interval keepalives_count user dbname; do
  printf "%-24s " "$k"
  PGPASSWORD=zeroship psql "host=127.0.0.1 port=5432 user=postgres dbname=zeroship $k=" \
    -tAc "select 1" 2>&1 | head -1 | sed "s/^psql: error: //"
done'
```

Measured 2026-08-26, verbatim from that second block:

| Input | Result |
| --- | --- |
| `port=` | `1` - the compiled default is substituted |
| `connect_timeout=` | `invalid integer value ""` |
| `keepalives_idle=`, `keepalives_interval=`, `keepalives_count=` | same |
| `keepalives=`, `tcp_user_timeout=` | same |
| `user=` | `FATAL: role "root" does not exist` |
| `dbname=` | `1` |

READ THE `user=` ROW CAREFULLY: it is a FAILURE that proves a SUCCESS. libpq
substituted the operating-system user - `root` in this container - and the
server then refused that role. It never tried an empty user. `dbname=` succeeds
because it defaults to the user, which here is the connecting `postgres`. Both
are the `pguser[0] == '\0'` / `dbName[0] == '\0'` arms of `pqConnectOptions2`.
Do not record this row as "user= is rejected"; the substitution is the finding,
and the role error is just this container's OS user.

`require_auth=` accepts empty despite looking like an enum - do not infer it
from the others.

**An empty value can only occur at the END of the string.** After `=` libpq
skips whitespace and takes what follows as the value, so `user= host=h` asks for
a user literally named `host=h` and sets no host, and `port= connect_timeout=`
hands `port` the text `connect_timeout=` to parse as an integer. Both are
libpq's behaviour and this crate reproduces them; they look like parser bugs and
are not. That is why a test for an empty numeric must use ONE trailing
parameter - a DSN with two "empty" numerics is measuring the swallow, not the
empty value.

## Probing what libpq NEGOTIATED, not just what it accepted

The `.invalid` read-out shows what libpq PARSED. For parameters that change
what is negotiated on the wire, `psql`'s `\conninfo` reports the outcome:

```bash
docker exec $C psql "host=127.0.0.1 port=5432 user=postgres dbname=zeroship" \
  -c '\conninfo' 2>&1 | grep -i protocol
```

Run the control first, or a field that always prints the same value proves
nothing. MEASURED 2026-08-25 against PostgreSQL 18.4:

| connection string | `\conninfo` reports |
| --- | --- |
| no protocol settings | `3.0` |
| `max_protocol_version=3.2` | `3.2` |
| `max_protocol_version=3.0` | `3.0` |

So the field tracks negotiation, and **libpq 18 defaults to protocol 3.0** -
3.2 is opt-in. It reports 3.0 against a 16.14 server too, and 16 cannot do
better anyway.

`min_protocol_version` is a REFUSAL floor rather than a preference, and its
error names both sides:

```
psql: error: connection to server ... failed: server only supports protocol
version 3.0, but "min_protocol_version" was set to 3.2
```

That makes it the discriminating probe for whether a driver IMPLEMENTS the
parameter or merely accepts it: an implementation that ignores the floor
CONNECTS to a 16.14 server instead of refusing.

There is NO server-side view of the negotiated version to check against -
`pg_stat_activity` has no such column and `pg_settings` carries only the TLS
`ssl_min/max_protocol_version`. `\conninfo` is libpq's own client-side report,
which is still an independent implementation to compare a driver against, but
it is not the server's word.

**Through a pooler the answer changes, and that is worth knowing before
assuming a default is safe.** Asking for 3.2 through PgBouncer reports 3.0
whether it fronts 16.14 or 18.4: the pooler does not speak 3.2 and negotiates
the client down rather than refusing. So requesting 3.2 by default is safe in
that shape - it simply does not take effect there.

## Where this is pinned

`libs/compio-postgres/src/service.rs`, in the tests below the parser - one test
per service-file row above. The conninfo rows are pinned in
`src/config.rs`, in the `empty_parameter_values` test module. The live
end-to-end case is
`a_service_written_in_libpq_s_awkward_forms_still_connects` in
`tests/service_live.rs`, which writes a working service in the awkward forms and
requires it to connect; it fails with `Undefined` on a parser that only accepts
the tidy shape.

## Empty numeric values are per-OPTION, not per-type (measured 2026-08-30)

A comment in `config.rs` asserted the rule was per-TYPE: "every numeric option
takes empty as 'not given' and uses its default". **That is wrong.** Measured
against the 16.14 libpq in `zs-cpg-types-5475`, against a REAL server (the
`.invalid` instrument cannot settle integers - see the warning above):

    port=''                 -> 1          connects, uses the compiled default
    port=          (at end) -> 1          same
    connect_timeout=''      -> invalid integer value "" for connection option
    keepalives_idle=        -> invalid integer value ""
    keepalives_interval=    -> invalid integer value ""
    connect_timeout=5       -> 1          control: the instrument works

`pg_config --configure` shows `--with-pgport=5432`, which is the default `port=`
selects. So `port` is a genuine one-option exception and every other numeric
option refuses an empty value.

### Two driver-specific options are outside libpq's opinion entirely

    statement_cache_capacity=  -> invalid connection option "statement_cache_capacity"
    max_message_size=          -> invalid connection option "max_message_size"

libpq does not know these keywords, so there is no libpq behaviour to match for
them and any rule we pick is our own design choice, not a compatibility
constraint. Do not cite "libpq does X" when arguing about them.

### The probe that looks empty and is not

    psql "host=127.0.0.1 port= user=postgres dbname=postgres"
    -> invalid integer value "user=postgres" for connection option "port"

libpq skips whitespace after `=` and takes the NEXT TOKEN as the value, so
`port= user=postgres` sets port to the string `user=postgres`. An empty value
must be written `port=''` or placed at the end of the string. A probe written
the first way is not testing what it appears to test - it silently becomes a
test of a completely different value, and the error message is the only tell.
