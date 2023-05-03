use std::borrow::Cow;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{bail, Context as _, Result};
use futures::stream::FuturesUnordered;
use futures::{ready, FutureExt as _, StreamExt as _};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite;
use tungstenite::protocol::frame::coding::CloseCode;

use super::handshake::{Protocol, WebSocket};
use super::{handshake, proto, session, Server, Upgrade};

/// State of a Hrana connection.
struct Conn {
    conn_id: u64,
    server: Arc<Server>,
    ws: WebSocket,
    ws_closed: bool,
    /// The version of the protocol that has been negotiated in the WebSocket handshake.
    protocol: Protocol,
    /// After a successful authentication, this contains the session-level state of the connection.
    session: Option<session::Session>,
    /// Join set for all tasks that were spawned to handle the connection.
    join_set: tokio::task::JoinSet<()>,
    /// Future responses to requests that we have received but are evaluating asynchronously.
    responses: FuturesUnordered<ResponseFuture>,
}

/// A `Future` that stores a handle to a future response to request which is being evaluated
/// asynchronously.
struct ResponseFuture {
    /// The request id, which must be included in the response.
    request_id: i32,
    /// The future that will be resolved with the response.
    response_rx: futures::future::Fuse<oneshot::Receiver<Result<proto::Response>>>,
}

/// A protocol error that should close the WebSocket.
#[derive(Debug)]
pub struct ProtocolError {
    pub code: CloseCode,
    pub message: String,
}

impl ProtocolError {
    pub fn from_message(message: impl Into<String>) -> Self {
        Self {
            code: CloseCode::Policy,
            message: message.into(),
        }
    }
}

pub(super) async fn handle_tcp(
    server: Arc<Server>,
    socket: tokio::net::TcpStream,
    conn_id: u64,
) -> Result<()> {
    let (ws, protocol) = handshake::handshake_tcp(socket)
        .await
        .context("Could not perform the WebSocket handshake on TCP connection")?;
    handle_ws(server, ws, protocol, conn_id).await
}

pub(super) async fn handle_upgrade(
    server: Arc<Server>,
    upgrade: Upgrade,
    conn_id: u64,
) -> Result<()> {
    let (ws, protocol) = handshake::handshake_upgrade(upgrade)
        .await
        .context("Could not perform the WebSocket handshake on HTTP connection")?;
    handle_ws(server, ws, protocol, conn_id).await
}

async fn handle_ws(
    server: Arc<Server>,
    ws: WebSocket,
    protocol: Protocol,
    conn_id: u64,
) -> Result<()> {
    let mut conn = Conn {
        conn_id,
        server,
        ws,
        ws_closed: false,
        protocol,
        session: None,
        join_set: tokio::task::JoinSet::new(),
        responses: FuturesUnordered::new(),
    };

    loop {
        if let Some(kicker) = conn.server.idle_kicker.as_ref() {
            kicker.kick();
        }

        tokio::select! {
            Some(client_msg_res) = conn.ws.recv() => {
                let client_msg = client_msg_res
                    .context("Could not receive a WebSocket message")?;
                match handle_msg(&mut conn, client_msg).await {
                    Ok(true) => continue,
                    Ok(false) => break,
                    Err(err) => {
                        match err.downcast::<ProtocolError>() {
                            Ok(proto_err) => {
                                tracing::warn!(
                                    "Connection #{} terminated due to protocol error: {}",
                                    conn.conn_id,
                                    proto_err.message,
                                );
                                close(&mut conn, proto_err.code, proto_err.message).await;
                                return Ok(())
                            }
                            Err(err) => {
                                close(&mut conn, CloseCode::Error, "Internal server error".into()).await;
                                return Err(err);
                            }
                        }
                    }
                }
            },
            Some(task_res) = conn.join_set.join_next() => {
                task_res.expect("Connection subtask failed")
            },
            Some(response_res) = conn.responses.next() => {
                let response_msg = response_res?;
                send_msg(&mut conn, &response_msg).await?;
            },
            else => break,
        }
    }

    close(
        &mut conn,
        CloseCode::Normal,
        "Thank you for using sqld".into(),
    )
    .await;
    Ok(())
}

async fn handle_msg(conn: &mut Conn, client_msg: tungstenite::Message) -> Result<bool> {
    match client_msg {
        tungstenite::Message::Text(client_msg) => {
            // client messages are received as text WebSocket messages that encode the `ClientMsg`
            // in JSON
            let client_msg: proto::ClientMsg = match serde_json::from_str(&client_msg) {
                Ok(client_msg) => client_msg,
                Err(err) => bail!(ProtocolError {
                    code: CloseCode::Invalid,
                    message: format!("Invalid format of client message: {err}"),
                }),
            };

            match client_msg {
                proto::ClientMsg::Hello { jwt } => handle_hello_msg(conn, jwt).await,
                proto::ClientMsg::Request {
                    request_id,
                    request,
                } => handle_request_msg(conn, request_id, request).await,
            }
        }
        tungstenite::Message::Ping(ping_data) => {
            let pong_msg = tungstenite::Message::Pong(ping_data);
            conn.ws
                .send(pong_msg)
                .await
                .context("Could not send pong to the WebSocket")?;
            Ok(true)
        }
        tungstenite::Message::Close(_) => Ok(false),
        _ => bail!(ProtocolError {
            code: CloseCode::Unsupported,
            message: "Received an unsupported WebSocket message".into(),
        }),
    }
}

async fn handle_hello_msg(conn: &mut Conn, jwt: Option<String>) -> Result<bool> {
    let hello_res = match conn.session.as_mut() {
        None => session::handle_initial_hello(&conn.server, conn.protocol, jwt)
            .map(|session| conn.session = Some(session)),
        Some(session) => session::handle_repeated_hello(&conn.server, session, jwt),
    };

    match hello_res {
        Ok(_) => {
            send_msg(conn, &proto::ServerMsg::HelloOk {}).await?;
            Ok(true)
        }
        Err(err) => match downcast_error(err) {
            Ok(error) => {
                send_msg(conn, &proto::ServerMsg::HelloError { error }).await?;
                Ok(false)
            }
            Err(err) => Err(err),
        },
    }
}

async fn handle_request_msg(
    conn: &mut Conn,
    request_id: i32,
    request: proto::Request,
) -> Result<bool> {
    let Some(session) = conn.session.as_mut() else {
        bail!(ProtocolError::from_message("Requests can only be sent after a hello"))
    };

    let response_rx = session::handle_request(&conn.server, session, &mut conn.join_set, request)
        .await
        .unwrap_or_else(|err| {
            // we got an error immediately, but let's treat it as a special case of the general
            // flow
            let (tx, rx) = oneshot::channel();
            tx.send(Err(err)).unwrap();
            rx
        });

    conn.responses.push(ResponseFuture {
        request_id,
        response_rx: response_rx.fuse(),
    });
    Ok(true)
}

impl Future for ResponseFuture {
    type Output = Result<proto::ServerMsg>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        match ready!(Pin::new(&mut self.response_rx).poll(cx)) {
            Ok(Ok(response)) => Poll::Ready(Ok(proto::ServerMsg::ResponseOk {
                request_id: self.request_id,
                response,
            })),
            Ok(Err(err)) => match downcast_error(err) {
                Ok(error) => Poll::Ready(Ok(proto::ServerMsg::ResponseError {
                    request_id: self.request_id,
                    error,
                })),
                Err(err) => Poll::Ready(Err(err)),
            },
            Err(_recv_err) => {
                // do not propagate this error, because the error that caused the receiver to drop
                // is very likely propagating from another task at this moment, and we don't want
                // to hide it.
                // this is also the reason why we need to use `Fuse` in self.response_rx
                tracing::warn!("Response sender was dropped");
                Poll::Pending
            }
        }
    }
}

fn downcast_error(err: anyhow::Error) -> Result<proto::Error> {
    match err.downcast_ref::<session::ResponseError>() {
        Some(error) => Ok(proto::Error {
            message: error.to_string(),
            code: error.code().into(),
        }),
        None => Err(err),
    }
}

async fn send_msg(conn: &mut Conn, msg: &proto::ServerMsg) -> Result<()> {
    let msg = serde_json::to_string(&msg).context("Could not serialize response message")?;
    let msg = tungstenite::Message::Text(msg);
    conn.ws
        .send(msg)
        .await
        .context("Could not send response to the WebSocket")
}

async fn close(conn: &mut Conn, code: CloseCode, reason: String) {
    if conn.ws_closed {
        return;
    }

    let close_frame = tungstenite::protocol::frame::CloseFrame {
        code,
        reason: Cow::Owned(reason),
    };
    if let Err(err) = conn
        .ws
        .send(tungstenite::Message::Close(Some(close_frame)))
        .await
    {
        if !matches!(
            err,
            tungstenite::Error::AlreadyClosed | tungstenite::Error::ConnectionClosed
        ) {
            tracing::warn!(
                "Could not send close frame to WebSocket of connection #{}: {:?}",
                conn.conn_id,
                err
            );
        }
    }

    conn.ws_closed = true;
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        <String as fmt::Display>::fmt(&self.message, f)
    }
}
