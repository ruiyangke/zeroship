//! Shared byte transport primitives for WebSocket and `node:net`.
//!
//! Protocol modules own their frame/event semantics. This module owns the
//! common byte pump mechanics: receive queue backpressure, pump wakeups, TCP
//! read/write halves, the plain/TLS stream enum, and the read-vs-command select
//! shape used by non-aliasable streams.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::rc::Rc;
use std::task::Waker;

use compio::buf::{IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite};
use compio::net::TcpStream;
#[cfg(feature = "runtime_native_websocket")]
use compio_tls::TlsStream;
use futures::future::{Either, select};

use crate::state::{OpResult, SharedState};

pub const RECV_BACKPRESSURE_CAP: usize = 256;
pub const RECV_BACKPRESSURE_RESUME: usize = 128;

pub trait RecvBackpressure {
    fn queued_event_len(&self) -> usize;

    fn recv_backpressure_waker_mut(&mut self) -> &mut Option<Waker>;

    fn recv_paused(&self) -> bool {
        false
    }

    fn recv_closed(&self) -> bool {
        false
    }
}

pub trait EventQueue<E>: RecvBackpressure {
    fn events_mut(&mut self) -> &mut VecDeque<E>;
}

pub fn enqueue_event<S, E>(queued: &Rc<RefCell<S>>, event: E)
where
    S: EventQueue<E>,
{
    queued.borrow_mut().events_mut().push_back(event);
}

pub fn schedule_event_op(state: &SharedState, op: OpResult) {
    let fut: std::pin::Pin<Box<dyn std::future::Future<Output = OpResult>>> =
        Box::pin(async move { op });
    state.borrow_mut().spawned_ops.push(fut);
    state.borrow().notify_pump();
}

pub fn drain_events<S, E>(queued: Option<Rc<RefCell<S>>>) -> Vec<E>
where
    S: EventQueue<E>,
{
    let Some(queued) = queued else {
        return Vec::new();
    };
    let drained: Vec<E> = {
        let mut s = queued.borrow_mut();
        s.events_mut().drain(..).collect()
    };
    let waker = {
        let mut s = queued.borrow_mut();
        if s.queued_event_len() < RECV_BACKPRESSURE_RESUME && !s.recv_paused() {
            s.recv_backpressure_waker_mut().take()
        } else {
            None
        }
    };
    if let Some(w) = waker {
        w.wake();
    }
    drained
}

pub async fn await_recv_ready<S>(queued: &Rc<RefCell<S>>) -> bool
where
    S: RecvBackpressure,
{
    use std::future::poll_fn;
    poll_fn(|cx| {
        let mut s = queued.borrow_mut();
        if s.recv_closed() {
            std::task::Poll::Ready(false)
        } else if !s.recv_paused() && s.queued_event_len() < RECV_BACKPRESSURE_CAP {
            std::task::Poll::Ready(true)
        } else {
            *s.recv_backpressure_waker_mut() = Some(cx.waker().clone());
            std::task::Poll::Pending
        }
    })
    .await
}

pub fn recv_ready_now<S>(queued: &Rc<RefCell<S>>) -> bool
where
    S: RecvBackpressure,
{
    let s = queued.borrow();
    !s.recv_closed() && !s.recv_paused() && s.queued_event_len() < RECV_BACKPRESSURE_CAP
}

#[allow(clippy::large_enum_variant)]
pub enum SocketStream {
    Plain(TcpStream),
    #[cfg(feature = "runtime_native_websocket")]
    Tls(TlsStream<TcpStream>),
    Closed,
}

impl AsyncRead for SocketStream {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> compio::buf::BufResult<usize, B> {
        match self {
            SocketStream::Plain(tcp) => tcp.read(buf).await,
            #[cfg(feature = "runtime_native_websocket")]
            SocketStream::Tls(tls) => tls.read(buf).await,
            SocketStream::Closed => compio::buf::BufResult(
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "socket closed")),
                buf,
            ),
        }
    }
}

impl AsyncWrite for SocketStream {
    async fn write<T: IoBuf>(&mut self, buf: T) -> compio::buf::BufResult<usize, T> {
        match self {
            SocketStream::Plain(tcp) => tcp.write(buf).await,
            #[cfg(feature = "runtime_native_websocket")]
            SocketStream::Tls(tls) => tls.write(buf).await,
            SocketStream::Closed => compio::buf::BufResult(
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "socket closed")),
                buf,
            ),
        }
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        match self {
            SocketStream::Plain(_) => Ok(()),
            #[cfg(feature = "runtime_native_websocket")]
            SocketStream::Tls(tls) => tls.flush().await,
            SocketStream::Closed => Ok(()),
        }
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        match self {
            SocketStream::Plain(tcp) => tcp.shutdown().await,
            #[cfg(feature = "runtime_native_websocket")]
            SocketStream::Tls(tls) => tls.shutdown().await,
            SocketStream::Closed => Ok(()),
        }
    }
}

pub struct TcpReadHalf {
    tcp: Rc<TcpStream>,
}

impl TcpReadHalf {
    pub fn new(tcp: Rc<TcpStream>) -> Self {
        Self { tcp }
    }
}

impl AsyncRead for TcpReadHalf {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> compio::buf::BufResult<usize, B> {
        let r: &TcpStream = &self.tcp;
        let mut r = r;
        r.read(buf).await
    }
}

pub struct TcpWriteHalf {
    pub tcp: Rc<TcpStream>,
}

impl TcpWriteHalf {
    pub fn new(tcp: Rc<TcpStream>) -> Self {
        Self { tcp }
    }
}

impl AsyncWrite for TcpWriteHalf {
    async fn write<T: IoBuf>(&mut self, buf: T) -> compio::buf::BufResult<usize, T> {
        let w: &TcpStream = &self.tcp;
        let mut w = w;
        w.write(buf).await
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        let w: &TcpStream = &self.tcp;
        let mut w = w;
        w.shutdown().await
    }
}

pub enum SelectAction<C> {
    ReadCompleted(compio::buf::BufResult<usize, Vec<u8>>),
    Command(Option<C>),
    ReadPermit(bool),
}

pub async fn select_read_or_command<R, S, F, C>(
    queued: &Rc<RefCell<S>>,
    reader: &mut R,
    command_fut: F,
    chunk_size: usize,
) -> SelectAction<C>
where
    R: AsyncRead + Unpin,
    S: RecvBackpressure,
    F: Future<Output = Option<C>>,
{
    if recv_ready_now(queued) {
        select_read_or_command_ready(reader, command_fut, chunk_size).await
    } else {
        let permit_fut = await_recv_ready(queued);
        let permit_fut = std::pin::pin!(permit_fut);
        let command_fut = std::pin::pin!(command_fut);
        match select(permit_fut, command_fut).await {
            Either::Left((ready, _)) => SelectAction::ReadPermit(ready),
            Either::Right((cmd, _)) => SelectAction::Command(cmd),
        }
    }
}

pub async fn select_read_or_command_ready<R, F, C>(
    reader: &mut R,
    command_fut: F,
    chunk_size: usize,
) -> SelectAction<C>
where
    R: AsyncRead + Unpin,
    F: Future<Output = Option<C>>,
{
    let chunk = vec![0u8; chunk_size];
    let read_fut = AsyncRead::read(reader, chunk);
    let read_fut = std::pin::pin!(read_fut);
    let command_fut = std::pin::pin!(command_fut);
    match select(read_fut, command_fut).await {
        Either::Left((res, _)) => SelectAction::ReadCompleted(res),
        Either::Right((cmd, _)) => SelectAction::Command(cmd),
    }
}
