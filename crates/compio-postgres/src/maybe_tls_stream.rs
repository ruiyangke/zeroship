// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Compio doesn't use pin-projection (async-fn traits carry no `poll_*`
// methods), so the wrapper is a plain enum; no pin_project_lite needed.

use crate::tls::{ChannelBinding, TlsStream};
use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite};
use std::io;

/// A stream that may or may not be TLS-wrapped.
pub enum MaybeTlsStream<S, T> {
    /// Plain underlying transport (TCP or Unix socket).
    Raw(S),
    /// TLS-wrapped stream produced by a [`TlsConnect`](crate::tls::TlsConnect).
    Tls(T),
}

impl<S, T> AsyncRead for MaybeTlsStream<S, T>
where
    S: AsyncRead + Unpin,
    T: AsyncRead + Unpin,
{
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        match self {
            MaybeTlsStream::Raw(s) => s.read(buf).await,
            MaybeTlsStream::Tls(s) => s.read(buf).await,
        }
    }
}

impl<S, T> AsyncWrite for MaybeTlsStream<S, T>
where
    S: AsyncWrite + Unpin,
    T: AsyncWrite + Unpin,
{
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        match self {
            MaybeTlsStream::Raw(s) => s.write(buf).await,
            MaybeTlsStream::Tls(s) => s.write(buf).await,
        }
    }

    async fn flush(&mut self) -> io::Result<()> {
        match self {
            MaybeTlsStream::Raw(s) => s.flush().await,
            MaybeTlsStream::Tls(s) => s.flush().await,
        }
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        match self {
            MaybeTlsStream::Raw(s) => s.shutdown().await,
            MaybeTlsStream::Tls(s) => s.shutdown().await,
        }
    }
}

impl<S, T> TlsStream for MaybeTlsStream<S, T>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsStream + Unpin,
{
    fn channel_binding(&self) -> ChannelBinding {
        match self {
            MaybeTlsStream::Raw(_) => ChannelBinding::none(),
            MaybeTlsStream::Tls(s) => s.channel_binding(),
        }
    }
}
