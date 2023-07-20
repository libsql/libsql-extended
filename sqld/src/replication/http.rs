use crate::replication::{frame::Frame, primary::frame_stream::FrameStream, ReplicationLogger};
use crate::Auth;
use anyhow::{Context, Result};
use hyper::server::conn::AddrIncoming;
use hyper::{Body, Method, Request, Response};
use std::net::SocketAddr;
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::trace::DefaultOnResponse;
use tower_http::{compression::CompressionLayer, cors};
use tracing::{Level, Span};

pub(crate) async fn run(
    auth: Arc<Auth>,
    addr: SocketAddr,
    logger: Arc<ReplicationLogger>,
) -> Result<()> {
    tracing::info!("listening for HTTP requests on {addr}");

    fn trace_request<B>(req: &Request<B>, _span: &Span) {
        tracing::debug!("got request: {} {}", req.method(), req.uri());
    }
    let service = ServiceBuilder::new()
        .layer(
            tower_http::trace::TraceLayer::new_for_http()
                .on_request(trace_request)
                .on_response(
                    DefaultOnResponse::new()
                        .level(Level::DEBUG)
                        .latency_unit(tower_http::LatencyUnit::Micros),
                ),
        )
        .layer(CompressionLayer::new())
        .layer(
            cors::CorsLayer::new()
                .allow_methods(cors::AllowMethods::any())
                .allow_headers(cors::Any)
                .allow_origin(cors::Any),
        )
        .service_fn(move |req| {
            let auth = auth.clone();
            let logger = logger.clone();
            handle_request(auth, req, logger)
        });

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let server = hyper::server::Server::builder(AddrIncoming::from_listener(listener)?)
        .tcp_nodelay(true)
        .serve(tower::make::Shared::new(service));

    server.await.context("Http server exited with an error")?;

    Ok(())
}

async fn handle_request(
    auth: Arc<Auth>,
    req: Request<Body>,
    logger: Arc<ReplicationLogger>,
) -> Result<Response<Body>> {
    let auth_header = req.headers().get(hyper::header::AUTHORIZATION);
    let auth = match auth.authenticate_http(auth_header) {
        Ok(auth) => auth,
        Err(err) => {
            return Ok(Response::builder()
                .status(hyper::StatusCode::UNAUTHORIZED)
                .body(err.to_string().into())
                .unwrap());
        }
    };

    match (req.method(), req.uri().path()) {
        (&Method::POST, "/frames") => handle_query(req, auth, logger).await,
        _ => Ok(Response::builder().status(404).body(Body::empty()).unwrap()),
    }
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct FramesRequest {
    pub next_offset: u64,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct Frames {
    pub frames: Vec<Frame>,
}

impl Frames {
    pub fn new() -> Self {
        Self { frames: Vec::new() }
    }

    pub fn push(&mut self, frame: Frame) {
        self.frames.push(frame);
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

fn error(msg: &str, code: hyper::StatusCode) -> Response<Body> {
    let err = serde_json::json!({ "error": msg });
    Response::builder()
        .status(code)
        .body(Body::from(serde_json::to_vec(&err).unwrap()))
        .unwrap()
}

async fn handle_query(
    mut req: Request<Body>,
    _auth: crate::auth::Authenticated,
    logger: Arc<ReplicationLogger>,
) -> Result<Response<Body>> {
    let bytes = hyper::body::to_bytes(req.body_mut()).await?;
    let FramesRequest { next_offset } = match serde_json::from_slice(&bytes) {
        Ok(req) => req,
        Err(resp) => return Ok(error(&resp.to_string(), hyper::StatusCode::BAD_REQUEST)),
    };
    tracing::trace!("Requested next offset: {next_offset}");

    let current_frameno = next_offset.saturating_sub(1);
    let mut frame_stream = FrameStream::new(logger, current_frameno);

    if frame_stream.max_available_frame_no < next_offset {
        tracing::trace!("No frames available starting {next_offset}, returning 204 No Content");
        return Ok(Response::builder()
            .status(hyper::StatusCode::NO_CONTENT)
            .body(Body::empty())
            .unwrap());
    }

    let mut frames = Frames::new();
    loop {
        use futures::StreamExt;

        match frame_stream.next().await {
            Some(Ok(frame)) => {
                tracing::trace!("Read frame {}", frame_stream.current_frame_no);
                frames.push(frame);
            }
            Some(Err(e)) => {
                tracing::error!("Error reading frame: {}", e);
                return Ok(Response::builder()
                    .status(hyper::StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::empty())
                    .unwrap());
            }
            None => break,
        }

        // FIXME: also stop when we have enough frames to fill a large buffer
        if frame_stream.max_available_frame_no <= frame_stream.current_frame_no {
            break;
        }
    }

    if frames.is_empty() {
        return Ok(Response::builder()
            .status(hyper::StatusCode::NO_CONTENT)
            .body(Body::empty())
            .unwrap());
    }

    Ok(Response::builder()
        .status(hyper::StatusCode::OK)
        .body(Body::from(serde_json::to_string(&frames)?))
        .unwrap())
}
