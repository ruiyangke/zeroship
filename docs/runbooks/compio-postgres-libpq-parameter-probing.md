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
ask for - or refused a file `psql` accepts. Measured 2026-08-25 against libpq
16.14.

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

**`key=` is per-TYPE, and this is the trap.** A numeric option takes empty as
"not given"; an enum REFUSES it; a string keeps it:

```bash
docker exec $C sh -c 'for k in port connect_timeout keepalives_idle sslmode \
    channel_binding target_session_attrs gssencmode load_balance_hosts; do
  printf "%-24s " "$k"
  psql "host=x.invalid $k=" -c "select 1" 2>&1 | head -1 | sed "s/^psql: error: //"
done'
```

Expect a host-resolution failure for the numeric ones and
`invalid <name> value: ""` for the enums. `require_auth=` accepts empty despite
looking like an enum - do not infer it from the others.

**An empty value can only occur at the END of the string.** After `=` libpq
skips whitespace and takes what follows as the value, so `user= host=h` asks for
a user literally named `host=h` and sets no host, and `port= connect_timeout=`
hands `port` the text `connect_timeout=` to parse as an integer. Both are
libpq's behaviour and this crate reproduces them; they look like parser bugs and
are not. That is why a test for an empty numeric must use ONE trailing
parameter - a DSN with two "empty" numerics is measuring the swallow, not the
empty value.

## Where this is pinned

`libs/compio-postgres/src/service.rs`, in the tests below the parser - one test
per service-file row above. The conninfo rows are pinned in
`src/config.rs`, in the `empty_parameter_values` test module. The live
end-to-end case is
`a_service_written_in_libpq_s_awkward_forms_still_connects` in
`tests/service_live.rs`, which writes a working service in the awkward forms and
requires it to connect; it fails with `Undefined` on a parser that only accepts
the tidy shape.
