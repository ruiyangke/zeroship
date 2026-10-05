//! A loopback proxy between a case's journal store and its `PostgreSQL` server.
//!
//! The store connects to the proxy rather than to the server, so every
//! statement the store sends crosses it. While armed, the proxy holds each
//! round trip for a set delay before the server sees it, which is what a
//! networked or loaded journal does to every statement, read or write alike,
//! and it records each statement the store executes by the first journal table
//! the statement names.

#![expect(
    clippy::future_not_send,
    reason = "the proxy runs on the case's own compio runtime"
)]

use compio::{
    buf::BufResult,
    io::{AsyncRead, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap},
    rc::Rc,
    time::Duration,
};

/// The most bytes one read carries across the proxy.
const CHUNK: usize = 64 * 1024;
/// The frontend message that ends an extended-protocol round trip.
const SYNC: u8 = b'S';
/// The frontend message that is a whole simple-protocol round trip.
const QUERY: u8 = b'Q';
const PARSE: u8 = b'P';
const BIND: u8 = b'B';
const EXECUTE: u8 = b'E';
/// The startup request codes after which the client sends another untyped
/// startup message rather than its first typed one.
const NEGOTIATION_CODES: [u32; 2] = [80_877_103, 80_877_104];
/// The subject of an Execute whose portal names no statement this connection
/// parsed: a framing fault in the proxy, named so a comparison shows it.
const UNMAPPED: &str = "<execute of an unparsed statement>";

/// What one armed window saw.
#[derive(Debug)]
pub(super) struct Recorded {
    /// Statements executed, by the first journal table each names, or by its
    /// command when it names none.
    pub statements: BTreeMap<String, usize>,
    /// Empty simple queries: the connection pool's checkout validation of a
    /// connection idle past its bypass window. They execute no statement, and
    /// how many a window sees depends on how long its connections sat idle.
    pub empty_queries: u32,
    /// Round trips the store began.
    pub round_trips: u32,
}

/// What one frontend message does, as far as the instrument is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    /// A statement executed, named by [`subject`].
    Statement(String),
    /// A simple query carrying no SQL.
    EmptyQuery,
    /// Anything else: startup, authentication, Parse, Bind, Describe, Sync.
    Other,
}

/// One whole frontend message and what it does.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Framed {
    bytes: Vec<u8>,
    event: Event,
    /// Whether the client waits for the server after this message.
    ends_round_trip: bool,
}

/// The frontend half of one connection: frames the client's byte stream into
/// whole messages whatever the read boundaries, and remembers which statement
/// each portal executes.
#[derive(Default)]
struct Frontend {
    started: bool,
    pending: Vec<u8>,
    statements: HashMap<String, String>,
    portals: HashMap<String, String>,
}

impl Frontend {
    /// Take the bytes of one read, whole messages or not.
    fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    /// The next whole message the reads so far carry.
    fn next(&mut self) -> Option<Framed> {
        let typed = self.started;
        let header = usize::from(typed);
        let length = self
            .pending
            .get(header..header + 4)
            .map(|bytes| u32::from_be_bytes(bytes.try_into().unwrap()))?;
        let total = header + usize::try_from(length).unwrap();
        if self.pending.len() < total {
            return None;
        }
        let bytes: Vec<u8> = self.pending.drain(..total).collect();
        if !typed {
            let code = bytes
                .get(4..8)
                .map_or(0, |code| u32::from_be_bytes(code.try_into().unwrap()));
            self.started = !NEGOTIATION_CODES.contains(&code);
            return Some(Framed {
                bytes,
                event: Event::Other,
                ends_round_trip: false,
            });
        }
        let kind = bytes[0];
        let body = &bytes[5..];
        let event = match kind {
            PARSE => {
                let [name, sql] = strings(body);
                self.statements.insert(name, sql);
                Event::Other
            }
            BIND => {
                let [portal, statement] = strings(body);
                self.portals.insert(portal, statement);
                Event::Other
            }
            EXECUTE => {
                let [portal, _] = strings(body);
                Event::Statement(
                    self.portals
                        .get(&portal)
                        .and_then(|statement| self.statements.get(statement))
                        .map_or_else(|| UNMAPPED.to_owned(), |sql| subject(sql)),
                )
            }
            QUERY => {
                let [sql, _] = strings(body);
                if sql.trim().is_empty() {
                    Event::EmptyQuery
                } else {
                    Event::Statement(subject(&sql))
                }
            }
            _ => Event::Other,
        };
        Some(Framed {
            bytes,
            event,
            ends_round_trip: matches!(kind, SYNC | QUERY),
        })
    }
}

#[derive(Default)]
struct Instrument {
    armed: Cell<bool>,
    delay: Cell<Duration>,
    statements: RefCell<BTreeMap<String, usize>>,
    empty_queries: Cell<u32>,
    round_trips: Cell<u32>,
}

impl Instrument {
    fn record(&self, event: &Event) {
        if !self.armed.get() {
            return;
        }
        match event {
            Event::Statement(subject) => {
                *self
                    .statements
                    .borrow_mut()
                    .entry(subject.clone())
                    .or_default() += 1;
            }
            Event::EmptyQuery => self.empty_queries.set(self.empty_queries.get() + 1),
            Event::Other => {}
        }
    }
}

pub(super) struct Wire {
    url: String,
    instrument: Rc<Instrument>,
}

impl Wire {
    /// Listen on a loopback port and carry every connection made to it to the
    /// server `url` names. The proxy runs on the case's own runtime.
    pub(super) async fn start(url: &str) -> Self {
        let upstream = url
            .split_once('@')
            .and_then(|(_, rest)| rest.split_once('/'))
            .map(|(address, _)| address.to_owned())
            .expect("a server URL names its address");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let instrument = Rc::new(Instrument::default());
        let shared = Rc::clone(&instrument);
        let target = upstream.clone();
        compio::runtime::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let server = TcpStream::connect(target.as_str()).await.unwrap();
                // The store's own sockets send without coalescing; the proxy's
                // must too, or each relayed round trip waits on a delayed ACK.
                client.set_nodelay(true).unwrap();
                server.set_nodelay(true).unwrap();
                compio::runtime::spawn(backend(server.clone(), client.clone())).detach();
                compio::runtime::spawn(frontend(client, server, Rc::clone(&shared))).detach();
            }
        })
        .detach();
        Self {
            url: url.replacen(&upstream, &format!("127.0.0.1:{port}"), 1),
            instrument,
        }
    }

    /// The server URL with the proxy in place of the server.
    pub(super) fn url(&self) -> &str {
        &self.url
    }

    /// Hold every round trip for `delay` and record statements until
    /// [`Wire::disarm`].
    pub(super) fn arm(&self, delay: Duration) {
        self.instrument.statements.borrow_mut().clear();
        self.instrument.empty_queries.set(0);
        self.instrument.round_trips.set(0);
        self.instrument.delay.set(delay);
        self.instrument.armed.set(true);
    }

    /// Stop holding and recording, and hand back what the armed window saw.
    pub(super) fn disarm(&self) -> Recorded {
        self.instrument.armed.set(false);
        Recorded {
            statements: self.instrument.statements.take(),
            empty_queries: self.instrument.empty_queries.get(),
            round_trips: self.instrument.round_trips.get(),
        }
    }
}

/// Carry the server's replies to the client unchanged.
async fn backend(mut server: TcpStream, mut client: TcpStream) {
    let mut buffer = vec![0_u8; CHUNK];
    loop {
        let BufResult(read, mut chunk) = server.read(buffer).await;
        let Ok(length @ 1..) = read else {
            return;
        };
        chunk.truncate(length);
        let BufResult(written, mut chunk) = client.write_all(chunk).await;
        if written.is_err() {
            return;
        }
        chunk.resize(CHUNK, 0);
        buffer = chunk;
    }
}

/// Carry the client's messages to the server, holding each round trip while
/// armed and recording what each message does.
async fn frontend(mut client: TcpStream, mut server: TcpStream, instrument: Rc<Instrument>) {
    let mut framing = Frontend::default();
    let mut buffer = vec![0_u8; CHUNK];
    loop {
        let BufResult(read, returned) = client.read(buffer).await;
        let Ok(length @ 1..) = read else {
            return;
        };
        framing.push(&returned[..length]);
        buffer = returned;
        let mut outgoing = Vec::new();
        while let Some(framed) = framing.next() {
            outgoing.extend_from_slice(&framed.bytes);
            instrument.record(&framed.event);
            if framed.ends_round_trip && instrument.armed.get() {
                instrument.round_trips.set(instrument.round_trips.get() + 1);
                compio::time::sleep(instrument.delay.get()).await;
                if !forward(&mut server, &mut outgoing).await {
                    return;
                }
            }
        }
        if !forward(&mut server, &mut outgoing).await {
            return;
        }
    }
}

/// The first journal table `sql` names, or its command when it names none.
fn subject(sql: &str) -> String {
    const PREFIX: &str = "\"__zeroship_workflow_";
    sql.find(PREFIX).map_or_else(
        || {
            sql.split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase()
        },
        |start| {
            sql[start + PREFIX.len()..]
                .split('"')
                .next()
                .unwrap_or_default()
                .to_owned()
        },
    )
}

/// Send what has accumulated, reporting whether the server still takes it.
async fn forward(server: &mut TcpStream, outgoing: &mut Vec<u8>) -> bool {
    if outgoing.is_empty() {
        return true;
    }
    let BufResult(written, _) = server.write_all(std::mem::take(outgoing)).await;
    written.is_ok()
}

/// The first two NUL-terminated strings of a message body.
fn strings(body: &[u8]) -> [String; 2] {
    let mut parts = body
        .split(|byte| *byte == 0)
        .map(|part| String::from_utf8_lossy(part).into_owned());
    [
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    ]
}

fn typed(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut message = vec![kind];
    message.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
    message.extend_from_slice(body);
    message
}

fn untyped(code: u32, rest: &[u8]) -> Vec<u8> {
    let mut message = u32::try_from(rest.len() + 8)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    message.extend_from_slice(&code.to_be_bytes());
    message.extend_from_slice(rest);
    message
}

fn nul(parts: &[&str]) -> Vec<u8> {
    parts
        .iter()
        .flat_map(|part| part.bytes().chain([0]))
        .collect()
}

/// A session as the store speaks it: negotiation and startup, a password, a
/// named prepare and its execution, simple queries, the pool's empty
/// validation query, an unnamed pipelined statement and termination.
fn session() -> Vec<u8> {
    let mut stream = untyped(NEGOTIATION_CODES[0], &[]);
    stream.extend(untyped(196_608, &nul(&["user", "zeroship_workflow", ""])));
    stream.extend(typed(b'p', &nul(&["secret"])));
    let named = "SELECT 1 FROM \"workflow_manager\".\"__zeroship_workflow_signals\"";
    stream.extend(typed(PARSE, &[nul(&["s1", named]), vec![0, 0]].concat()));
    stream.extend(typed(b'D', &nul(&["Ss1"])));
    stream.extend(typed(SYNC, &[]));
    stream.extend(typed(BIND, &[nul(&["", "s1"]), vec![0; 6]].concat()));
    stream.extend(typed(EXECUTE, &[nul(&[""]), vec![0; 4]].concat()));
    stream.extend(typed(SYNC, &[]));
    stream.extend(typed(QUERY, &nul(&["BEGIN"])));
    stream.extend(typed(QUERY, &nul(&[""])));
    let unnamed = "UPDATE \"workflow_manager\".\"__zeroship_workflow_runs\" SET x = 1";
    stream.extend(typed(PARSE, &[nul(&["", unnamed]), vec![0, 0]].concat()));
    stream.extend(typed(BIND, &[nul(&["", ""]), vec![0; 6]].concat()));
    stream.extend(typed(EXECUTE, &[nul(&[""]), vec![0; 4]].concat()));
    stream.extend(typed(SYNC, &[]));
    stream.extend(typed(b'X', &[]));
    stream
}

/// Frame `reads` as one connection receives them, in order.
fn frame(reads: &[&[u8]]) -> Vec<Framed> {
    let mut framing = Frontend::default();
    let mut framed = Vec::new();
    for read in reads {
        framing.push(read);
        while let Some(message) = framing.next() {
            framed.push(message);
        }
    }
    assert!(framing.pending.is_empty(), "a message was left unframed");
    framed
}

/// The whole session in one read names every statement, the empty query and
/// each round trip, and relays every byte in order.
#[test]
fn a_session_frames_into_its_statements_and_round_trips() {
    let stream = session();
    let framed = frame(&[&stream]);
    let events: Vec<_> = framed
        .iter()
        .filter(|message| message.event != Event::Other)
        .map(|message| message.event.clone())
        .collect();
    assert_eq!(
        events,
        [
            Event::Statement("signals".into()),
            Event::Statement("BEGIN".into()),
            Event::EmptyQuery,
            Event::Statement("runs".into()),
        ]
    );
    assert_eq!(
        framed
            .iter()
            .filter(|message| message.ends_round_trip)
            .count(),
        5
    );
    let relayed: Vec<u8> = framed
        .iter()
        .flat_map(|message| message.bytes.iter().copied())
        .collect();
    assert_eq!(relayed, stream);
}

/// Splitting the session at any byte, or delivering it one byte per read,
/// frames exactly what one read does.
#[test]
fn framing_is_the_same_at_every_read_boundary() {
    let stream = session();
    let whole = frame(&[&stream]);
    assert!(!whole.is_empty());
    for split in 1..stream.len() {
        let reads = [&stream[..split], &stream[split..]];
        assert_eq!(frame(&reads), whole, "split at {split}");
    }
    let bytes: Vec<&[u8]> = stream.chunks(1).collect();
    assert_eq!(frame(&bytes), whole, "one byte per read");
}
