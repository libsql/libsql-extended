use std::error::Error as StdError;
use std::io::Error as IoError;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use hyper::server::accept::Accept as HyperAccept;
use hyper::Uri;
use pin_project_lite::pin_project;
use tokio::io::{AsyncRead, AsyncWrite};
use tonic::transport::server::{Connected, TcpConnectInfo};
use tower::make::MakeConnection;

pub trait Connector:
    MakeConnection<Uri, Connection = Self::Conn, Future = Self::Fut, Error = Self::Err> + Send + 'static
{
    type Conn: Unpin + Send + 'static;
    type Fut: Send + 'static;
    type Err: StdError + Send + Sync;
}

impl<T> Connector for T
where
    T: MakeConnection<Uri> + Send + 'static,
    T::Connection: Unpin + Send + 'static,
    T::Future: Send + 'static,
    T::Error: StdError + Send + Sync,
{
    type Conn = Self::Connection;
    type Fut = Self::Future;
    type Err = Self::Error;
}

pub trait Conn:
    AsyncRead + AsyncWrite + Unpin + Send + 'static + Connected<ConnectInfo = TcpConnectInfo>
{
}

pub trait Accept: HyperAccept<Conn = Self::Connection, Error = IoError> + Send + 'static {
    type Connection: Conn;
}

pub struct AddrIncoming {
    listener: tokio::net::TcpListener,
}

impl AddrIncoming {
    pub fn new(listener: tokio::net::TcpListener) -> Self {
        Self { listener }
    }
}

impl HyperAccept for AddrIncoming {
    type Conn = AddrStream;
    type Error = IoError;

    fn poll_accept(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Conn, Self::Error>>> {
        match ready!(self.listener.poll_accept(cx)) {
            Ok((stream, remote_addr)) => {
                // disable naggle algorithm
                stream.set_nodelay(true)?;
                let local_addr = stream.local_addr()?;
                Poll::Ready(Some(Ok(AddrStream {
                    stream,
                    local_addr,
                    remote_addr,
                })))
            }
            Err(e) => Poll::Ready(Some(Err(e))),
        }
    }
}

pin_project! {
    pub struct AddrStream {
        #[pin]
        stream: tokio::net::TcpStream,
        remote_addr: SocketAddr,
        local_addr: SocketAddr,
    }
}

impl Accept for AddrIncoming {
    type Connection = AddrStream;
}

impl Conn for AddrStream {}

impl AsyncRead for AddrStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.project().stream.poll_read(cx, buf)
    }
}

impl AsyncWrite for AddrStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        self.project().stream.poll_write(cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.project().stream.poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.project().stream.poll_shutdown(cx)
    }
}

impl Connected for AddrStream {
    type ConnectInfo = TcpConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.stream.connect_info()
    }
}
