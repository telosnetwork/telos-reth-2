//! Loopback HTTP entrypoint for the Telos retained-history JSON-RPC router.

use clap::Parser;
use http::{header::CONTENT_TYPE, Method, Request, Response, StatusCode};
use http_body::Body;
use http_body_util::BodyExt;
use jsonrpsee_server::{serve, HttpBody};
use serde_json::{json, Value};
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};
use telos_rpc_router::{readiness, BackendConfig, ReadinessConfig, RouterConfig, RpcRouter};
use tokio::{
    net::TcpListener,
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tower::service_fn;
use tracing::{error, info, warn};
use url::Url;

const APPLICATION_JSON: &str = "application/json";

#[derive(Debug, Parser)]
#[command(name = "telos-rpc-router")]
struct Command {
    /// Loopback listener used by the external TLS/rate-limit proxy.
    #[arg(long, env = "TELOS_RPC_ROUTER_LISTEN", default_value = "127.0.0.1:8645")]
    listen: SocketAddr,
    /// Live Telos Reth v2 JSON-RPC URL.
    #[arg(long, env = "TELOS_RPC_ROUTER_LIVE_URL")]
    live_url: Url,
    /// Retained-history incumbent JSON-RPC URL.
    #[arg(long, env = "TELOS_RPC_ROUTER_ARCHIVE_URL")]
    archive_url: Url,
    /// First block available from the sparse live database.
    #[arg(long, env = "TELOS_RPC_ROUTER_LIVE_HISTORY_START")]
    live_history_start: u64,
    /// Expected EVM chain ID.
    #[arg(long, env = "TELOS_RPC_ROUTER_CHAIN_ID")]
    chain_id: u64,
    /// Exact block hash at `live-history-start`.
    #[arg(long, env = "TELOS_RPC_ROUTER_ANCHOR_HASH")]
    anchor_hash: String,
    /// Retained block used to prove that the archive backend has pre-checkpoint history.
    #[arg(long, env = "TELOS_RPC_ROUTER_HISTORY_PROBE_NUMBER")]
    history_probe_number: u64,
    /// Exact hash of `history-probe-number`.
    #[arg(long, env = "TELOS_RPC_ROUTER_HISTORY_PROBE_HASH")]
    history_probe_hash: String,
    /// Account whose balance is pinned at the retained-history probe block.
    #[arg(long, env = "TELOS_RPC_ROUTER_HISTORY_PROBE_ADDRESS")]
    history_probe_address: String,
    /// Exact balance expected for `history-probe-address`.
    #[arg(long, env = "TELOS_RPC_ROUTER_HISTORY_PROBE_BALANCE")]
    history_probe_balance: String,
    /// Transaction whose receipt must belong to the retained-history probe block.
    #[arg(long, env = "TELOS_RPC_ROUTER_HISTORY_PROBE_TRANSACTION_HASH")]
    history_probe_transaction_hash: String,
    /// Maximum allowed height difference between live and archive backends.
    #[arg(long, env = "TELOS_RPC_ROUTER_MAX_HEAD_LAG", default_value_t = 4)]
    max_head_lag: u64,
    /// Maximum inbound JSON-RPC request size in bytes.
    #[arg(long, env = "TELOS_RPC_ROUTER_MAX_REQUEST_BYTES", default_value_t = 15 * 1024 * 1024)]
    max_request_bytes: usize,
    /// Maximum backend JSON-RPC response size in bytes.
    #[arg(long, env = "TELOS_RPC_ROUTER_MAX_RESPONSE_BYTES", default_value_t = 64 * 1024 * 1024)]
    max_response_bytes: usize,
    /// Maximum number of requests accepted in one JSON-RPC batch.
    #[arg(long, env = "TELOS_RPC_ROUTER_MAX_BATCH_LEN", default_value_t = 64)]
    max_batch_len: usize,
    /// Maximum concurrent requests shared by both backends.
    #[arg(long, env = "TELOS_RPC_ROUTER_MAX_INFLIGHT", default_value_t = 256)]
    max_inflight: usize,
    /// Maximum number of simultaneous inbound HTTP connections.
    #[arg(long, env = "TELOS_RPC_ROUTER_MAX_CONNECTIONS", default_value_t = 256)]
    max_connections: usize,
    /// Maximum time allowed to receive an entire request body, in milliseconds.
    #[arg(long, env = "TELOS_RPC_ROUTER_REQUEST_TIMEOUT_MS", default_value_t = 30_000)]
    request_timeout_ms: u64,
    /// Backend timeout in milliseconds.
    #[arg(long, env = "TELOS_RPC_ROUTER_BACKEND_TIMEOUT_MS", default_value_t = 30_000)]
    backend_timeout_ms: u64,
}

#[derive(Clone)]
struct State {
    router: RpcRouter,
    chain_id: u64,
    anchor_hash: Arc<str>,
    history_probe_number: u64,
    history_probe_hash: Arc<str>,
    history_probe_address: Arc<str>,
    history_probe_balance: Arc<str>,
    history_probe_transaction_hash: Arc<str>,
    max_head_lag: u64,
    max_request_bytes: usize,
    request_timeout: Duration,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "telos_rpc_router=info".into()),
        )
        .init();
    if let Err(error) = run(Command::parse()).await {
        error!(%error, "router terminated");
        std::process::exit(1);
    }
}

async fn run(command: Command) -> Result<(), Box<dyn std::error::Error>> {
    validate_ingress(&command)?;
    let connection_permits = Arc::new(Semaphore::new(command.max_connections));
    let config = RouterConfig {
        live_history_start: command.live_history_start,
        max_response_bytes: command.max_response_bytes,
        max_batch_len: command.max_batch_len,
        max_inflight: command.max_inflight,
        backend_timeout: Duration::from_millis(command.backend_timeout_ms),
    };
    let router = RpcRouter::new(
        config,
        BackendConfig { name: "live", url: command.live_url },
        BackendConfig { name: "archive", url: command.archive_url },
    )?;
    let state = State {
        router,
        chain_id: command.chain_id,
        anchor_hash: command.anchor_hash.into(),
        history_probe_number: command.history_probe_number,
        history_probe_hash: command.history_probe_hash.into(),
        history_probe_address: command.history_probe_address.into(),
        history_probe_balance: command.history_probe_balance.into(),
        history_probe_transaction_hash: command.history_probe_transaction_hash.into(),
        max_head_lag: command.max_head_lag,
        max_request_bytes: command.max_request_bytes,
        request_timeout: Duration::from_millis(command.request_timeout_ms),
    };
    readiness_response(&state).await?;

    let listener = TcpListener::bind(command.listen).await?;
    info!(listen = %listener.local_addr()?, "Telos RPC history router ready");
    loop {
        let (stream, remote) = listener.accept().await?;
        if !remote.ip().is_loopback() {
            warn!(%remote, "rejected non-loopback connection");
            continue
        }
        let Some(connection_permit) = try_acquire_connection(&connection_permits) else {
            warn!(%remote, "rejected connection because the router is at capacity");
            continue
        };
        let state = state.clone();
        let service = service_fn(move |request| {
            let state = state.clone();
            async move { Ok::<_, Infallible>(handle(request, state).await) }
        });
        tokio::spawn(async move {
            let _connection_permit = connection_permit;
            if let Err(error) = serve(stream, service).await {
                warn!(%error, "HTTP connection failed");
            }
        });
    }
}

async fn handle(request: Request<hyper::body::Incoming>, state: State) -> Response<HttpBody> {
    match (request.method(), request.uri().path()) {
        (&Method::GET, "/readyz") => match readiness_response(&state).await {
            Ok(value) => json_response(StatusCode::OK, value),
            Err(error) => json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"ready": false, "error": error.to_string()}),
            ),
        },
        (&Method::POST, "/") => {
            let Some(content_type) = request.headers().get(CONTENT_TYPE) else {
                return json_response(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    json!({"error": "content-type must be application/json"}),
                )
            };
            if !content_type
                .to_str()
                .is_ok_and(|value| value.split(';').next() == Some(APPLICATION_JSON))
            {
                return json_response(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    json!({"error": "content-type must be application/json"}),
                )
            }
            if request
                .body()
                .size_hint()
                .upper()
                .is_some_and(|length| length > state.max_request_bytes as u64)
            {
                return json_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    json!({"error": "request exceeds configured size limit"}),
                )
            }
            let bytes = match read_request_body_with_timeout(
                request.into_body(),
                state.max_request_bytes,
                state.request_timeout,
            )
            .await
            {
                Ok(bytes) => bytes,
                Err(RequestBodyError::Read) => {
                    return json_response(
                        StatusCode::BAD_REQUEST,
                        json!({"error": "failed to read request"}),
                    )
                }
                Err(RequestBodyError::TooLarge) => {
                    return json_response(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        json!({"error": "request exceeds configured size limit"}),
                    )
                }
                Err(RequestBodyError::TimedOut) => {
                    return json_response(
                        StatusCode::REQUEST_TIMEOUT,
                        json!({"error": "request body timed out"}),
                    )
                }
            };
            let payload: Value = match serde_json::from_slice(&bytes) {
                Ok(payload) => payload,
                Err(_) => {
                    return json_response(
                        StatusCode::BAD_REQUEST,
                        json!({
                            "jsonrpc": "2.0",
                            "id": null,
                            "error": {"code": -32700, "message": "parse error"},
                        }),
                    )
                }
            };
            match state.router.dispatch(payload).await {
                Some(response) => json_response(StatusCode::OK, response),
                None => Response::builder()
                    .status(StatusCode::NO_CONTENT)
                    .body(HttpBody::empty())
                    .expect("valid no-content response"),
            }
        }
        _ => json_response(StatusCode::NOT_FOUND, json!({"error": "not found"})),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestBodyError {
    Read,
    TooLarge,
    TimedOut,
}

async fn read_request_body_with_timeout<B>(
    body: B,
    max_request_bytes: usize,
    request_timeout: Duration,
) -> Result<Vec<u8>, RequestBodyError>
where
    B: Body<Data = hyper::body::Bytes> + Unpin,
{
    match tokio::time::timeout(request_timeout, read_request_body(body, max_request_bytes)).await {
        Ok(result) => result,
        Err(_) => Err(RequestBodyError::TimedOut),
    }
}

async fn read_request_body<B>(
    mut body: B,
    max_request_bytes: usize,
) -> Result<Vec<u8>, RequestBodyError>
where
    B: Body<Data = hyper::body::Bytes> + Unpin,
{
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| RequestBodyError::Read)?;
        if let Ok(chunk) = frame.into_data() {
            if bytes.len().saturating_add(chunk.len()) > max_request_bytes {
                return Err(RequestBodyError::TooLarge)
            }
            bytes.extend_from_slice(&chunk);
        }
    }
    Ok(bytes)
}

fn try_acquire_connection(permits: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    permits.clone().try_acquire_owned().ok()
}

async fn readiness_response(state: &State) -> Result<Value, telos_rpc_router::RouterError> {
    readiness(
        &state.router,
        ReadinessConfig {
            expected_chain_id: state.chain_id,
            anchor_hash: &state.anchor_hash,
            history_probe_number: state.history_probe_number,
            history_probe_hash: &state.history_probe_hash,
            history_probe_address: &state.history_probe_address,
            history_probe_balance: &state.history_probe_balance,
            history_probe_transaction_hash: &state.history_probe_transaction_hash,
            max_head_lag: state.max_head_lag,
        },
    )
    .await
}

fn validate_ingress(command: &Command) -> Result<(), Box<dyn std::error::Error>> {
    if !command.listen.ip().is_loopback() {
        return Err("router listener must be loopback".into())
    }
    if command.max_request_bytes == 0 {
        return Err("maximum request size must be nonzero".into())
    }
    if command.max_connections == 0 {
        return Err("maximum connections must be nonzero".into())
    }
    if command.max_connections > Semaphore::MAX_PERMITS {
        return Err("maximum connections exceeds the runtime semaphore limit".into())
    }
    if command.max_inflight > Semaphore::MAX_PERMITS {
        return Err("maximum inflight requests exceeds the runtime semaphore limit".into())
    }
    if command.request_timeout_ms == 0 {
        return Err("request timeout must be nonzero".into())
    }
    Ok(())
}

fn json_response(status: StatusCode, value: Value) -> Response<HttpBody> {
    let body =
        serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"error\":\"encode failed\"}".to_vec());
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, APPLICATION_JSON)
        .body(HttpBody::from(body))
        .expect("valid JSON response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{stream, StreamExt};
    use http_body_util::StreamBody;
    use hyper::body::{Bytes, Frame};

    const PROBE_HASH: &str = "0x9af24c613ebf3ba3cbd8a29d9b4c24a0cf5589544a162dfe66c98f25a1ce55c0";
    const PROBE_ADDRESS: &str = "0x1a7883121285dfe08fb89763d084d5c7966dcf92";
    const PROBE_TRANSACTION: &str =
        "0x411b585bf0b052f527b1924f500686d4b7c7cab9da18f81cbacfa4405bd15819";

    fn command() -> Command {
        Command::try_parse_from([
            "telos-rpc-router",
            "--live-url",
            "http://127.0.0.1:20545/",
            "--archive-url",
            "http://127.0.0.1:8545/",
            "--live-history-start",
            "479294328",
            "--chain-id",
            "40",
            "--anchor-hash",
            "0x86e6ab6e5a81737240c22c24408d3c6af47050e466ac760e288b07ea392117a4",
            "--history-probe-number",
            "423015017",
            "--history-probe-hash",
            PROBE_HASH,
            "--history-probe-address",
            PROBE_ADDRESS,
            "--history-probe-balance",
            "0x23b0c973e84998e4f",
            "--history-probe-transaction-hash",
            PROBE_TRANSACTION,
            "--max-batch-len",
            "32",
            "--max-connections",
            "128",
            "--request-timeout-ms",
            "2500",
        ])
        .expect("valid command")
    }

    #[test]
    fn parses_history_pins_and_ingress_limits() {
        let command = command();

        assert_eq!(command.history_probe_address, PROBE_ADDRESS);
        assert_eq!(command.history_probe_balance, "0x23b0c973e84998e4f");
        assert_eq!(command.history_probe_transaction_hash, PROBE_TRANSACTION);
        assert_eq!(command.max_batch_len, 32);
        assert_eq!(command.max_connections, 128);
        assert_eq!(command.request_timeout_ms, 2500);
        assert!(validate_ingress(&command).is_ok());
    }

    #[test]
    fn rejects_unsafe_ingress_configuration() {
        let mut command = command();
        command.listen = "0.0.0.0:8645".parse().expect("valid address");
        assert!(validate_ingress(&command).is_err());

        command.listen = "127.0.0.1:8645".parse().expect("valid address");
        command.max_connections = 0;
        assert!(validate_ingress(&command).is_err());

        command.max_connections = 1;
        command.request_timeout_ms = 0;
        assert!(validate_ingress(&command).is_err());

        command.request_timeout_ms = 1;
        command.max_connections = Semaphore::MAX_PERMITS + 1;
        assert!(validate_ingress(&command).is_err());
    }

    #[tokio::test]
    async fn incomplete_stream_hits_the_whole_body_deadline() {
        let partial =
            stream::iter([Ok::<_, Infallible>(Frame::data(Bytes::from_static(b"{\"jsonrpc\":")))]);
        let incomplete = partial.chain(stream::pending());
        let body = StreamBody::new(incomplete);

        let result = read_request_body_with_timeout(body, 1024, Duration::from_millis(1)).await;

        assert_eq!(result, Err(RequestBodyError::TimedOut));
    }

    #[tokio::test]
    async fn streamed_body_is_rejected_as_soon_as_aggregate_limit_is_exceeded() {
        let chunks = stream::iter([
            Ok::<_, Infallible>(Frame::data(Bytes::from_static(b"1234"))),
            Ok::<_, Infallible>(Frame::data(Bytes::from_static(b"56"))),
        ]);
        let body = StreamBody::new(chunks);

        let result = read_request_body_with_timeout(body, 5, Duration::from_secs(1)).await;

        assert_eq!(result, Err(RequestBodyError::TooLarge));
    }

    #[test]
    fn connection_capacity_rejects_then_recovers_without_waiting() {
        let permits = Arc::new(Semaphore::new(1));
        let first = try_acquire_connection(&permits).expect("first connection is admitted");

        assert!(try_acquire_connection(&permits).is_none());

        drop(first);
        assert!(try_acquire_connection(&permits).is_some());
    }
}
