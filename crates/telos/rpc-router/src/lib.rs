//! Fail-closed JSON-RPC routing between a live Telos Reth v2 node and its retained history node.

use futures::{future::join_all, StreamExt};
use reqwest::{redirect::Policy, Client, Url};
use serde_json::{json, Map, Value};
use std::{
    collections::BTreeSet,
    io::{self, Write},
    net::IpAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Semaphore;

const JSONRPC_VERSION: &str = "2.0";
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const ROUTER_ERROR: i64 = -32070;
const RESPONSE_LIMIT_MESSAGE: &str = "response exceeds configured size limit";
// Keep this aligned with reth_rpc_server_types::constants::DEFAULT_MAX_STORAGE_VALUES_SLOTS.
const MAX_STORAGE_VALUES_SLOTS: usize = 1024;
const MAX_ARCHIVE_STORAGE_CALLS_PER_DISPATCH: usize = MAX_STORAGE_VALUES_SLOTS;

/// Immutable chain boundary and resource limits for the history router.
#[derive(Clone, Debug)]
pub struct RouterConfig {
    /// First block retained by the live Reth v2 database.
    pub live_history_start: u64,
    /// Maximum aggregate backend response bytes accepted by one dispatch or readiness probe.
    pub max_response_bytes: usize,
    /// Maximum number of requests accepted in one JSON-RPC batch.
    pub max_batch_len: usize,
    /// Maximum concurrent backend requests.
    pub max_inflight: usize,
    /// Whole backend-call timeout, including time waiting for a concurrency permit.
    pub backend_timeout: Duration,
}

impl RouterConfig {
    /// Validates resource limits and the nonzero history boundary.
    pub fn validate(&self) -> Result<(), RouterError> {
        if self.live_history_start == 0 {
            return Err(RouterError::Configuration("live history start must be nonzero".to_owned()))
        }
        if self.max_response_bytes == 0 {
            return Err(RouterError::Configuration(
                "maximum response size must be nonzero".to_owned(),
            ))
        }
        if self.max_response_bytes < encoded_json_len(&response_limit_error()) {
            return Err(RouterError::Configuration(
                "maximum response size cannot encode the router limit error".to_owned(),
            ))
        }
        if self.max_batch_len == 0 {
            return Err(RouterError::Configuration(
                "maximum batch length must be nonzero".to_owned(),
            ))
        }
        if self.max_inflight == 0 {
            return Err(RouterError::Configuration(
                "maximum inflight requests must be nonzero".to_owned(),
            ))
        }
        if self.backend_timeout.is_zero() {
            return Err(RouterError::Configuration("backend timeout must be nonzero".to_owned()))
        }
        Ok(())
    }
}

/// A pinned backend endpoint.
#[derive(Clone, Debug)]
pub struct BackendConfig {
    /// Stable diagnostic name. It is safe to expose in errors.
    pub name: &'static str,
    /// Exact JSON-RPC URL.
    pub url: Url,
}

impl BackendConfig {
    /// Requires an exact credential-free loopback HTTP URL.
    pub fn validate(&self) -> Result<(), RouterError> {
        if self.url.scheme() != "http" {
            return Err(RouterError::Configuration(format!(
                "{} backend must use loopback HTTP",
                self.name
            )))
        }
        let host =
            self.url.host_str().and_then(|host| host.parse::<IpAddr>().ok()).ok_or_else(|| {
                RouterError::Configuration(format!(
                    "{} backend host must be an explicit IP address",
                    self.name
                ))
            })?;
        if !host.is_loopback() || self.url.port().is_none() {
            return Err(RouterError::Configuration(format!(
                "{} backend must use an explicit loopback address and port",
                self.name
            )))
        }
        if !self.url.username().is_empty() || self.url.password().is_some() {
            return Err(RouterError::Configuration(format!(
                "{} backend URL must not contain credentials",
                self.name
            )))
        }
        if self.url.query().is_some() || self.url.fragment().is_some() {
            return Err(RouterError::Configuration(format!(
                "{} backend URL must not contain a query or fragment",
                self.name
            )))
        }
        if self.url.path() != "/" {
            return Err(RouterError::Configuration(format!(
                "{} backend URL path must be /",
                self.name
            )))
        }
        Ok(())
    }
}

/// Router failures that cannot be represented by a backend JSON-RPC response.
#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    /// Invalid immutable configuration.
    #[error("configuration error: {0}")]
    Configuration(String),
    /// Backend transport or response validation failed.
    #[error("{backend} backend error: {message}")]
    Backend {
        /// Backend diagnostic name.
        backend: &'static str,
        /// Sanitized failure description.
        message: String,
    },
}

#[derive(Clone, Debug)]
struct Backend {
    config: BackendConfig,
    client: Client,
    timeout: Duration,
    permits: Arc<Semaphore>,
}

#[derive(Debug)]
struct ResponseBudget {
    remaining: AtomicUsize,
    archive_storage_calls_remaining: AtomicUsize,
}

impl ResponseBudget {
    const fn new(limit: usize) -> Self {
        Self {
            remaining: AtomicUsize::new(limit),
            archive_storage_calls_remaining: AtomicUsize::new(
                MAX_ARCHIVE_STORAGE_CALLS_PER_DISPATCH,
            ),
        }
    }

    fn can_fit(&self, length: u64) -> bool {
        usize::try_from(length).is_ok_and(|length| length <= self.remaining.load(Ordering::Acquire))
    }

    fn consume(&self, length: usize) -> bool {
        self.remaining
            .try_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(length)
            })
            .is_ok()
    }

    fn reserve_archive_storage_calls(&self, calls: usize) -> bool {
        self.archive_storage_calls_remaining
            .try_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(calls)
            })
            .is_ok()
    }
}

impl Backend {
    fn new(
        config: BackendConfig,
        router: &RouterConfig,
        permits: Arc<Semaphore>,
    ) -> Result<Self, RouterError> {
        config.validate()?;
        let client = Client::builder()
            .redirect(Policy::none())
            .no_proxy()
            .timeout(router.backend_timeout)
            .build()
            .map_err(|error| RouterError::Configuration(error.to_string()))?;
        Ok(Self { config, client, timeout: router.backend_timeout, permits })
    }

    async fn call(
        &self,
        request: &Value,
        expects_response: bool,
        budget: &ResponseBudget,
    ) -> Result<Option<Value>, RouterError> {
        match tokio::time::timeout(
            self.timeout,
            self.call_within_timeout(request, expects_response, budget),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(RouterError::Backend {
                backend: self.config.name,
                message: "request timed out".to_owned(),
            }),
        }
    }

    async fn call_within_timeout(
        &self,
        request: &Value,
        expects_response: bool,
        budget: &ResponseBudget,
    ) -> Result<Option<Value>, RouterError> {
        let _permit = self.permits.acquire().await.map_err(|_| RouterError::Backend {
            backend: self.config.name,
            message: "request limiter is closed".to_owned(),
        })?;
        let body = serde_json::to_vec(request).map_err(|error| RouterError::Backend {
            backend: self.config.name,
            message: format!("failed to encode request: {error}"),
        })?;
        let response = self
            .client
            .post(self.config.url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|error| RouterError::Backend {
                backend: self.config.name,
                message: transport_error(&error),
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(RouterError::Backend {
                backend: self.config.name,
                message: format!("returned HTTP {status}"),
            })
        }
        if let Some(length) = response.content_length() &&
            !budget.can_fit(length)
        {
            return Err(RouterError::Backend {
                backend: self.config.name,
                message: "aggregate response exceeds configured size limit".to_owned(),
            })
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| RouterError::Backend {
                backend: self.config.name,
                message: transport_error(&error),
            })?;
            if !budget.consume(chunk.len()) {
                return Err(RouterError::Backend {
                    backend: self.config.name,
                    message: "aggregate response exceeds configured size limit".to_owned(),
                })
            }
            bytes.extend_from_slice(&chunk);
        }
        if !expects_response {
            if bytes.is_empty() {
                return Ok(None)
            }
            return Err(RouterError::Backend {
                backend: self.config.name,
                message: "returned a response to a notification".to_owned(),
            })
        }
        let response = serde_json::from_slice(&bytes).map_err(|error| RouterError::Backend {
            backend: self.config.name,
            message: format!("returned invalid JSON: {error}"),
        })?;
        let expected_id = request.get("id").ok_or_else(|| RouterError::Backend {
            backend: self.config.name,
            message: "response-bearing request has no id".to_owned(),
        })?;
        validate_backend_response(&response, expected_id).map_err(|message| {
            RouterError::Backend { backend: self.config.name, message: message.to_owned() }
        })?;
        Ok(Some(response))
    }
}

fn transport_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "request timed out".to_owned()
    } else if error.is_connect() {
        "connection failed".to_owned()
    } else {
        "transport failed".to_owned()
    }
}

/// A deterministic JSON-RPC router for a post-checkpoint live node and a retained history node.
#[derive(Clone, Debug)]
pub struct RpcRouter {
    config: RouterConfig,
    live: Backend,
    archive: Backend,
}

impl RpcRouter {
    /// Creates a router. Redirects and ambient proxy settings are disabled for both backends.
    pub fn new(
        config: RouterConfig,
        live: BackendConfig,
        archive: BackendConfig,
    ) -> Result<Self, RouterError> {
        config.validate()?;
        live.validate()?;
        archive.validate()?;
        if live.url == archive.url {
            return Err(RouterError::Configuration(
                "live and archive backends must use different normalized URLs".to_owned(),
            ))
        }
        let permits = Arc::new(Semaphore::new(config.max_inflight));
        let live = Backend::new(live, &config, permits.clone())?;
        let archive = Backend::new(archive, &config, permits)?;
        Ok(Self { config, live, archive })
    }

    /// Handles one JSON-RPC request or batch. `None` is returned for notifications-only input.
    pub async fn dispatch(&self, payload: Value) -> Option<Value> {
        let budget = Arc::new(ResponseBudget::new(self.config.max_response_bytes));
        self.dispatch_with_budget(payload, budget).await
    }

    async fn dispatch_with_budget(
        &self,
        payload: Value,
        budget: Arc<ResponseBudget>,
    ) -> Option<Value> {
        let response = match payload {
            Value::Array(requests) if requests.is_empty() => {
                Some(error_response(Value::Null, INVALID_REQUEST, "empty JSON-RPC batch"))
            }
            Value::Array(requests) if requests.len() > self.config.max_batch_len => {
                Some(error_response(
                    Value::Null,
                    INVALID_REQUEST,
                    "JSON-RPC batch exceeds configured maximum",
                ))
            }
            Value::Array(requests) => {
                let responses = join_all(requests.into_iter().map(|request| {
                    let router = self.clone();
                    let budget = budget.clone();
                    async move { router.dispatch_one(request, budget).await }
                }))
                .await
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
                (!responses.is_empty()).then_some(Value::Array(responses))
            }
            request => self.dispatch_one(request, budget).await,
        };
        response.map(|response| self.enforce_encoded_response_limit(response))
    }

    fn enforce_encoded_response_limit(&self, response: Value) -> Value {
        if encoded_json_len(&response) <= self.config.max_response_bytes {
            response
        } else {
            response_limit_error()
        }
    }

    async fn dispatch_one(&self, request: Value, budget: Arc<ResponseBudget>) -> Option<Value> {
        let Some(object) = request.as_object() else {
            return Some(error_response(Value::Null, INVALID_REQUEST, "request must be an object"))
        };
        let id = match object.get("id") {
            Some(id) if is_valid_id(id) => Some(id.clone()),
            Some(_) => {
                return Some(error_response(
                    Value::Null,
                    INVALID_REQUEST,
                    "id must be a string, number, or null",
                ))
            }
            None => None,
        };
        let response_id = id.clone().unwrap_or(Value::Null);
        if object.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
            return Some(error_response(response_id, INVALID_REQUEST, "jsonrpc must equal 2.0"))
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            return Some(error_response(response_id, INVALID_REQUEST, "method must be a string"))
        };
        let expects_response = id.is_some();
        let params = object.get("params").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
        if !params.is_array() {
            return expects_response.then(|| {
                error_response(
                    response_id,
                    INVALID_PARAMS,
                    "Ethereum RPC params must be positional",
                )
            })
        }
        let plan = match route_plan(method, &params, self.config.live_history_start) {
            Ok(plan) => plan,
            Err(RouteError::MethodNotFound) => {
                return expects_response.then(|| {
                    error_response(
                        response_id,
                        METHOD_NOT_FOUND,
                        "method is not exposed by this router",
                    )
                })
            }
            Err(RouteError::InvalidParams(message)) => {
                return expects_response
                    .then(|| error_response(response_id, INVALID_PARAMS, message))
            }
        };
        let result = self.execute(plan, request, expects_response, &budget).await;
        match result {
            Ok(None) => None,
            Ok(Some(response)) => Some(response),
            Err(error) => expects_response
                .then(|| error_response(response_id, ROUTER_ERROR, error.to_string())),
        }
    }

    async fn execute(
        &self,
        plan: RoutePlan,
        request: Value,
        expects_response: bool,
        budget: &ResponseBudget,
    ) -> Result<Option<Value>, RouterError> {
        match plan {
            RoutePlan::Live => self.live.call(&request, expects_response, budget).await,
            RoutePlan::Archive => self.archive.call(&request, expects_response, budget).await,
            RoutePlan::ArchiveStorageValues(plan) => {
                self.execute_archive_storage_values(&request, plan, expects_response, budget).await
            }
            RoutePlan::LiveThenArchive => {
                let response = self.live.call(&request, expects_response, budget).await?;
                if expects_response && response.as_ref().is_some_and(has_null_result) {
                    self.archive.call(&request, true, budget).await
                } else {
                    Ok(response)
                }
            }
            RoutePlan::Logs(plan) => {
                self.execute_logs(request, plan, expects_response, budget).await
            }
        }
    }

    async fn execute_archive_storage_values(
        &self,
        request: &Value,
        plan: StorageValuesPlan,
        expects_response: bool,
        budget: &ResponseBudget,
    ) -> Result<Option<Value>, RouterError> {
        let backend_calls = plan.total_slots.max(1);
        if !budget.reserve_archive_storage_calls(backend_calls) {
            return Err(RouterError::Backend {
                backend: "archive",
                message: "aggregate historical storage fan-out exceeds 1024 backend calls"
                    .to_owned(),
            })
        }
        if plan.total_slots == 0 {
            return self
                .execute_archive_empty_storage_values(request, plan, expects_response, budget)
                .await
        }

        let original_id = request.get("id").cloned().unwrap_or(Value::Null);
        let mut grouped = plan
            .entries
            .iter()
            .map(|entry| (entry.address.clone(), vec![Value::Null; entry.slots.len()]))
            .collect::<Vec<_>>();

        let archive = &self.archive;
        let concurrency = self.config.max_inflight;
        let block = plan.block;
        let calls = plan
            .entries
            .iter()
            .enumerate()
            .flat_map(|(entry_index, entry)| {
                entry.slots.iter().enumerate().map(move |(slot_index, slot)| {
                    (entry_index, slot_index, entry.address.clone(), slot.clone())
                })
            })
            .collect::<Vec<_>>();
        let responses = tokio::time::timeout(self.config.backend_timeout, async move {
            futures::stream::iter(calls.into_iter().enumerate())
                .map(move |(flat_index, (entry_index, slot_index, address, slot))| {
                    let block = block.clone();
                    async move {
                        let mut storage_request = json!({
                            "jsonrpc": JSONRPC_VERSION,
                            "method": "eth_getStorageAt",
                            "params": [address, slot],
                        });
                        if let Some(block) = block {
                            storage_request
                                .get_mut("params")
                                .and_then(Value::as_array_mut)
                                .expect("storage request has positional params")
                                .push(block);
                        }
                        if expects_response {
                            storage_request
                                .as_object_mut()
                                .expect("storage request is an object")
                                .insert(
                                    "id".to_owned(),
                                    Value::String(format!("telos-router-storage-{flat_index}")),
                                );
                        }
                        (
                            flat_index,
                            entry_index,
                            slot_index,
                            archive.call(&storage_request, expects_response, budget).await,
                        )
                    }
                })
                .buffer_unordered(concurrency)
                .collect::<Vec<_>>()
                .await
        })
        .await
        .map_err(|_| RouterError::Backend {
            backend: "archive",
            message: "storage values request timed out".to_owned(),
        })?;

        let mut responses = responses;
        responses.sort_unstable_by_key(|(flat_index, ..)| *flat_index);
        for (_, entry_index, slot_index, response) in responses {
            let response = response?;
            if !expects_response {
                if response.is_some() {
                    return Err(RouterError::Backend {
                        backend: "archive",
                        message: "storage notification returned a response".to_owned(),
                    })
                }
                continue
            }
            let response = response.ok_or_else(|| missing_response("archive"))?;
            if let Some(error) = copied_backend_error(&response, &original_id, "archive")? {
                return Ok(Some(error))
            }
            let value = response.get("result").and_then(Value::as_str).ok_or_else(|| {
                RouterError::Backend {
                    backend: "archive",
                    message: "eth_getStorageAt result is not a storage value".to_owned(),
                }
            })?;
            let value = normalize_storage_value(value)?;
            grouped[entry_index].1[slot_index] = Value::String(value);
        }
        if !expects_response {
            return Ok(None)
        }

        let mut result = Map::new();
        for (address, values) in grouped {
            if values.iter().any(Value::is_null) {
                return Err(RouterError::Backend {
                    backend: "archive",
                    message: "storage values result is incomplete".to_owned(),
                })
            }
            result.insert(address, Value::Array(values));
        }
        Ok(Some(json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": original_id,
            "result": result,
        })))
    }

    async fn execute_archive_empty_storage_values(
        &self,
        request: &Value,
        plan: StorageValuesPlan,
        expects_response: bool,
        budget: &ResponseBudget,
    ) -> Result<Option<Value>, RouterError> {
        let original_id = request.get("id").cloned().unwrap_or(Value::Null);
        let first = plan
            .entries
            .first()
            .expect("validated storage values request has at least one address");
        let mut probe = json!({
            "jsonrpc": JSONRPC_VERSION,
            "method": "eth_getBalance",
            "params": [first.address.as_str()],
        });
        if let Some(block) = &plan.block {
            probe
                .get_mut("params")
                .and_then(Value::as_array_mut)
                .expect("balance probe has positional params")
                .push(block.clone());
        }
        if expects_response {
            probe.as_object_mut().expect("balance probe is an object").insert(
                "id".to_owned(),
                Value::String("telos-router-storage-empty-probe".to_owned()),
            );
        }
        let response = self.archive.call(&probe, expects_response, budget).await?;
        if !expects_response {
            return Ok(None)
        }
        let response = response.ok_or_else(|| missing_response("archive"))?;
        if let Some(error) = copied_backend_error(&response, &original_id, "archive")? {
            return Ok(Some(error))
        }
        let balance =
            response.get("result").and_then(Value::as_str).ok_or_else(|| RouterError::Backend {
                backend: "archive",
                message: "empty storage-map balance probe result is not a quantity".to_owned(),
            })?;
        validate_unbounded_quantity(balance).map_err(|_| RouterError::Backend {
            backend: "archive",
            message: "empty storage-map balance probe result is not a valid quantity".to_owned(),
        })?;

        let result = plan
            .entries
            .into_iter()
            .map(|entry| (entry.address, Value::Array(Vec::new())))
            .collect::<Map<_, _>>();
        Ok(Some(json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": original_id,
            "result": result,
        })))
    }

    async fn execute_logs(
        &self,
        request: Value,
        plan: LogsPlan,
        expects_response: bool,
        budget: &ResponseBudget,
    ) -> Result<Option<Value>, RouterError> {
        match plan {
            LogsPlan::Live => self.live.call(&request, expects_response, budget).await,
            LogsPlan::Archive => self.archive.call(&request, expects_response, budget).await,
            LogsPlan::ByHash(hash) => {
                let probe = json!({
                    "jsonrpc": JSONRPC_VERSION,
                    "id": "telos-router-block-probe",
                    "method": "eth_getBlockByHash",
                    "params": [hash, false],
                });
                let response = self.live.call(&probe, true, budget).await?;
                let response = response.ok_or_else(|| missing_response("live"))?;
                if block_probe_is_absent(&response, &hash)? {
                    self.archive.call(&request, expects_response, budget).await
                } else {
                    self.live.call(&request, expects_response, budget).await
                }
            }
            LogsPlan::Split { archive_to, live_from } => {
                if !expects_response {
                    // Ethereum log queries are not useful as notifications, but preserve
                    // notification semantics and ensure both non-overlapping
                    // ranges are evaluated.
                    let archive_request = replace_log_range(
                        &request,
                        None,
                        Some(archive_to),
                        self.config.live_history_start,
                    )?;
                    let live_request = replace_log_range(
                        &request,
                        Some(live_from),
                        None,
                        self.config.live_history_start,
                    )?;
                    let (archive, live) = tokio::join!(
                        self.archive.call(&archive_request, false, budget),
                        self.live.call(&live_request, false, budget)
                    );
                    archive?;
                    live?;
                    return Ok(None)
                }
                let archive_request = replace_log_range(
                    &request,
                    None,
                    Some(archive_to),
                    self.config.live_history_start,
                )?;
                let live_request = replace_log_range(
                    &request,
                    Some(live_from),
                    None,
                    self.config.live_history_start,
                )?;
                let (archive, live) = tokio::join!(
                    self.archive.call(&archive_request, true, budget),
                    self.live.call(&live_request, true, budget)
                );
                merge_log_responses(
                    archive?.ok_or_else(|| missing_response("archive"))?,
                    live?.ok_or_else(|| missing_response("live"))?,
                    archive_to,
                    live_from,
                )
                .map(Some)
            }
        }
    }
}

fn missing_response(backend: &'static str) -> RouterError {
    RouterError::Backend { backend, message: "returned no JSON-RPC response".to_owned() }
}

const fn is_valid_id(id: &Value) -> bool {
    matches!(id, Value::Null | Value::String(_) | Value::Number(_))
}

fn validate_backend_response(response: &Value, expected_id: &Value) -> Result<(), &'static str> {
    let Some(object) = response.as_object() else {
        return Err("backend response is not an object")
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
        return Err("backend response has an invalid jsonrpc version")
    }
    if object.get("id") != Some(expected_id) {
        return Err("backend response id does not match the request")
    }
    if object.contains_key("result") == object.contains_key("error") {
        return Err("backend response must contain exactly one of result or error")
    }
    if let Some(error) = object.get("error") {
        validate_backend_error(error)?;
    }
    Ok(())
}

fn validate_backend_error(error: &Value) -> Result<(), &'static str> {
    let Some(error) = error.as_object() else {
        return Err("backend response error is not an object")
    };
    if error.get("code").and_then(Value::as_i64).is_none() {
        return Err("backend response error code is not an integer")
    }
    if error.get("message").and_then(Value::as_str).is_none() {
        return Err("backend response error message is not a string")
    }
    Ok(())
}

fn copied_backend_error(
    response: &Value,
    original_id: &Value,
    backend: &'static str,
) -> Result<Option<Value>, RouterError> {
    let Some(error) = response.get("error") else { return Ok(None) };
    validate_backend_error(error)
        .map_err(|message| RouterError::Backend { backend, message: message.to_owned() })?;
    Ok(Some(json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": original_id,
        "error": error,
    })))
}

fn has_null_result(response: &Value) -> bool {
    response.get("result").is_some_and(Value::is_null)
}

fn block_probe_is_absent(response: &Value, expected_hash: &str) -> Result<bool, RouterError> {
    if response.get("error").is_some() {
        return Err(RouterError::Backend {
            backend: "live",
            message: "block-hash routing probe returned a JSON-RPC error".to_owned(),
        })
    }
    if has_null_result(response) {
        return Ok(true)
    }
    let hash = response.pointer("/result/hash").and_then(Value::as_str).ok_or_else(|| {
        RouterError::Backend {
            backend: "live",
            message: "block-hash routing probe returned an invalid block".to_owned(),
        }
    })?;
    parse_hash(hash).map_err(|_| RouterError::Backend {
        backend: "live",
        message: "block-hash routing probe returned an invalid block hash".to_owned(),
    })?;
    if !hash.eq_ignore_ascii_case(expected_hash) {
        return Err(RouterError::Backend {
            backend: "live",
            message: "block-hash routing probe returned a different block".to_owned(),
        })
    }
    Ok(false)
}

fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": {
            "code": code,
            "message": message.into(),
        }
    })
}

fn response_limit_error() -> Value {
    error_response(Value::Null, ROUTER_ERROR, RESPONSE_LIMIT_MESSAGE)
}

#[derive(Default)]
struct EncodedLength(usize);

impl Write for EncodedLength {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encoded_json_len(value: &Value) -> usize {
    let mut length = EncodedLength::default();
    serde_json::to_writer(&mut length, value)
        .expect("JSON values serialize to an infallible writer");
    length.0
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RoutePlan {
    Live,
    Archive,
    ArchiveStorageValues(StorageValuesPlan),
    LiveThenArchive,
    Logs(LogsPlan),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LogsPlan {
    Live,
    Archive,
    ByHash(String),
    Split { archive_to: u64, live_from: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RouteError {
    MethodNotFound,
    InvalidParams(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum BlockRef {
    Number(u64),
    Earliest,
    LiveTag,
    Hash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StorageValuesPlan {
    entries: Vec<StorageValuesEntry>,
    block: Option<Value>,
    total_slots: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StorageValuesEntry {
    address: String,
    slots: Vec<String>,
}

fn route_plan(method: &str, params: &Value, live_start: u64) -> Result<RoutePlan, RouteError> {
    let params = params.as_array().expect("validated positional params");

    if matches!(
        method,
        "eth_chainId" |
            "eth_blockNumber" |
            "eth_baseFee" |
            "eth_gasPrice" |
            "eth_maxPriorityFeePerGas" |
            "eth_accounts" |
            "eth_sendRawTransaction" |
            "net_version" |
            "net_peerCount" |
            "web3_sha3"
    ) {
        return Ok(RoutePlan::Live)
    }

    if matches!(
        method,
        "eth_newFilter" |
            "eth_newBlockFilter" |
            "eth_newPendingTransactionFilter" |
            "eth_getFilterChanges" |
            "eth_getFilterLogs" |
            "eth_uninstallFilter" |
            "eth_feeHistory"
    ) {
        // Filter IDs are backend-local. Keeping the complete lifecycle on the incumbent avoids
        // ambiguous IDs while both nodes run side by side. feeHistory ranges can cross the
        // checkpoint, so the live incumbent is also authoritative for that method.
        return Ok(RoutePlan::Archive)
    }

    if method == "eth_getLogs" {
        return route_logs(params, live_start).map(RoutePlan::Logs)
    }

    if matches!(
        method,
        "eth_getBlockByHash" |
            "eth_getHeaderByHash" |
            "eth_getBlockTransactionCountByHash" |
            "eth_getTransactionByBlockHashAndIndex" |
            "eth_getUncleByBlockHashAndIndex" |
            "eth_getUncleCountByBlockHash" |
            "eth_getTransactionByHash" |
            "eth_getTransactionReceipt" |
            "eth_getRawTransactionByHash" |
            "eth_getRawTransactionByBlockHashAndIndex"
    ) {
        return Ok(RoutePlan::LiveThenArchive)
    }

    if matches!(
        method,
        "eth_getBlockByNumber" |
            "eth_getHeaderByNumber" |
            "eth_getBlockTransactionCountByNumber" |
            "eth_getTransactionByBlockNumberAndIndex" |
            "eth_getUncleByBlockNumberAndIndex" |
            "eth_getUncleCountByBlockNumber" |
            "eth_getRawTransactionByBlockNumberAndIndex" |
            "eth_getBlockReceipts"
    ) {
        return route_at(params.first(), live_start)
    }

    if matches!(
        method,
        "eth_getBalance" |
            "eth_getTransactionCount" |
            "eth_getCode" |
            "eth_call" |
            "eth_estimateGas"
    ) {
        return route_optional_state_at(params.get(1), live_start)
    }

    if method == "eth_getStorageValues" {
        return route_storage_values(params, live_start)
    }

    if method == "eth_getStorageAt" {
        return route_optional_state_at(params.get(2), live_start)
    }

    Err(RouteError::MethodNotFound)
}

fn route_at(value: Option<&Value>, live_start: u64) -> Result<RoutePlan, RouteError> {
    let value = value.ok_or(RouteError::InvalidParams("missing block reference"))?;
    match parse_block_ref(value)? {
        BlockRef::Number(number) if number < live_start => Ok(RoutePlan::Archive),
        BlockRef::Earliest => Ok(RoutePlan::Archive),
        BlockRef::Number(_) | BlockRef::LiveTag => Ok(RoutePlan::Live),
        BlockRef::Hash => Ok(RoutePlan::LiveThenArchive),
    }
}

fn route_optional_state_at(
    value: Option<&Value>,
    live_start: u64,
) -> Result<RoutePlan, RouteError> {
    match value {
        Some(value) => match parse_block_ref(value)? {
            BlockRef::Number(number) if number < live_start => Ok(RoutePlan::Archive),
            BlockRef::Earliest | BlockRef::Hash => Ok(RoutePlan::Archive),
            BlockRef::Number(_) | BlockRef::LiveTag => Ok(RoutePlan::Live),
        },
        None => Ok(RoutePlan::Live),
    }
}

fn route_storage_values(params: &[Value], live_start: u64) -> Result<RoutePlan, RouteError> {
    let plan = parse_storage_values_plan(params)?;
    match route_optional_state_at(params.get(1), live_start)? {
        RoutePlan::Archive => Ok(RoutePlan::ArchiveStorageValues(plan)),
        RoutePlan::Live => Ok(RoutePlan::Live),
        _ => unreachable!("state routing returns only live or archive"),
    }
}

fn parse_storage_values_plan(params: &[Value]) -> Result<StorageValuesPlan, RouteError> {
    if !(1..=2).contains(&params.len()) {
        return Err(RouteError::InvalidParams(
            "eth_getStorageValues requires requests and an optional block reference",
        ))
    }
    let requests = params
        .first()
        .and_then(Value::as_object)
        .ok_or(RouteError::InvalidParams("eth_getStorageValues requests must be an object"))?;
    if requests.is_empty() {
        return Err(RouteError::InvalidParams("eth_getStorageValues requests must not be empty"))
    }
    if requests.len() > MAX_STORAGE_VALUES_SLOTS {
        return Err(RouteError::InvalidParams("eth_getStorageValues exceeds the 1024 address limit"))
    }

    let mut seen = BTreeSet::new();
    let mut entries = Vec::with_capacity(requests.len());
    let mut total_slots = 0usize;
    for (address, slots) in requests {
        parse_address(address)?;
        let address = address.to_ascii_lowercase();
        if !seen.insert(address.clone()) {
            return Err(RouteError::InvalidParams(
                "eth_getStorageValues contains duplicate normalized addresses",
            ))
        }
        let slots = slots.as_array().ok_or(RouteError::InvalidParams(
            "eth_getStorageValues address values must be slot arrays",
        ))?;
        total_slots = total_slots.checked_add(slots.len()).ok_or(RouteError::InvalidParams(
            "eth_getStorageValues exceeds the total slot limit",
        ))?;
        if total_slots > MAX_STORAGE_VALUES_SLOTS {
            return Err(RouteError::InvalidParams(
                "eth_getStorageValues exceeds the 1024 total slot limit",
            ))
        }
        let slots = slots
            .iter()
            .map(|slot| {
                let slot = slot.as_str().ok_or(RouteError::InvalidParams(
                    "eth_getStorageValues slots must be strings",
                ))?;
                validate_storage_key(slot)?;
                Ok(slot.to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        entries.push(StorageValuesEntry { address, slots });
    }
    Ok(StorageValuesPlan { entries, block: params.get(1).cloned(), total_slots })
}

fn parse_block_ref(value: &Value) -> Result<BlockRef, RouteError> {
    match value {
        Value::String(value) => {
            if value == "earliest" {
                Ok(BlockRef::Earliest)
            } else if matches!(value.as_str(), "latest" | "safe" | "finalized" | "pending") {
                Ok(BlockRef::LiveTag)
            } else if value.len() == 66 {
                parse_hash(value)?;
                Ok(BlockRef::Hash)
            } else {
                parse_quantity(value).map(BlockRef::Number)
            }
        }
        Value::Object(object) => {
            let number = object.get("blockNumber");
            let hash = object.get("blockHash");
            if number.is_some() == hash.is_some() {
                return Err(RouteError::InvalidParams(
                    "EIP-1898 reference must contain exactly one blockNumber or blockHash",
                ))
            }
            if let Some(number) = number {
                parse_block_ref(number)
            } else {
                let hash = hash
                    .and_then(Value::as_str)
                    .ok_or(RouteError::InvalidParams("EIP-1898 blockHash must be a string"))?;
                parse_hash(hash)?;
                Ok(BlockRef::Hash)
            }
        }
        _ => Err(RouteError::InvalidParams("block reference must be a string or object")),
    }
}

fn parse_hash(value: &str) -> Result<(), RouteError> {
    let digits = value
        .strip_prefix("0x")
        .ok_or(RouteError::InvalidParams("block hash must be 0x-prefixed"))?;
    if digits.len() != 64 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RouteError::InvalidParams("block hash must contain exactly 32 bytes"))
    }
    Ok(())
}

fn parse_address(value: &str) -> Result<(), RouteError> {
    let digits =
        value.strip_prefix("0x").ok_or(RouteError::InvalidParams("address must be 0x-prefixed"))?;
    if digits.len() != 40 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RouteError::InvalidParams("address must contain exactly 20 bytes"))
    }
    Ok(())
}

fn validate_storage_key(value: &str) -> Result<(), RouteError> {
    let digits = value
        .strip_prefix("0x")
        .ok_or(RouteError::InvalidParams("storage key must be 0x-prefixed"))?;
    if digits.is_empty() ||
        digits.len() > 64 ||
        !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(RouteError::InvalidParams(
            "storage key must contain between 1 and 64 hexadecimal digits",
        ))
    }
    Ok(())
}

fn validate_storage_value(value: &str) -> Result<(), RouteError> {
    let digits = value
        .strip_prefix("0x")
        .ok_or(RouteError::InvalidParams("storage value must be 0x-prefixed"))?;
    if digits.len() != 64 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RouteError::InvalidParams("storage value must contain exactly 32 bytes"))
    }
    Ok(())
}

fn normalize_storage_value(value: &str) -> Result<String, RouterError> {
    validate_storage_value(value).map_err(|_| RouterError::Backend {
        backend: "archive",
        message: "eth_getStorageAt result is not exactly 32 bytes".to_owned(),
    })?;
    let digits = value.strip_prefix("0x").expect("validated storage value prefix");
    Ok(format!("0x{}", digits.to_ascii_lowercase()))
}

fn validate_unbounded_quantity(value: &str) -> Result<(), RouteError> {
    let digits = value
        .strip_prefix("0x")
        .ok_or(RouteError::InvalidParams("quantity must be 0x-prefixed"))?;
    if digits.is_empty() ||
        (digits.len() > 1 && digits.starts_with('0')) ||
        !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(RouteError::InvalidParams("invalid quantity"))
    }
    Ok(())
}

fn parse_quantity(value: &str) -> Result<u64, RouteError> {
    validate_unbounded_quantity(value)?;
    let digits = value.strip_prefix("0x").expect("validated prefix");
    u64::from_str_radix(digits, 16)
        .map_err(|_| RouteError::InvalidParams("block quantity exceeds u64"))
}

fn route_logs(params: &[Value], live_start: u64) -> Result<LogsPlan, RouteError> {
    let filter = params
        .first()
        .and_then(Value::as_object)
        .ok_or(RouteError::InvalidParams("eth_getLogs requires one filter object"))?;
    if let Some(block_hash) = filter.get("blockHash") {
        if filter.contains_key("fromBlock") || filter.contains_key("toBlock") {
            return Err(RouteError::InvalidParams(
                "blockHash cannot be combined with fromBlock or toBlock",
            ))
        }
        let block_hash =
            block_hash.as_str().ok_or(RouteError::InvalidParams("blockHash must be a string"))?;
        parse_hash(block_hash)?;
        return Ok(LogsPlan::ByHash(block_hash.to_owned()))
    }

    let from =
        filter.get("fromBlock").map(parse_log_bound).transpose()?.unwrap_or(LogBound::Latest);
    let to = filter.get("toBlock").map(parse_log_bound).transpose()?.unwrap_or(LogBound::Latest);
    match (&from, &to) {
        (LogBound::Number(from), LogBound::Number(to)) if from > to => {
            Err(RouteError::InvalidParams("fromBlock exceeds toBlock"))
        }
        (from, to) if tag_order(from).zip(tag_order(to)).is_some_and(|(from, to)| from > to) => {
            Err(RouteError::InvalidParams("fromBlock exceeds toBlock"))
        }
        (
            LogBound::Pending,
            LogBound::Number(_) | LogBound::Finalized | LogBound::Safe | LogBound::Latest,
        ) => Err(RouteError::InvalidParams("fromBlock exceeds toBlock")),
        (LogBound::Finalized | LogBound::Safe | LogBound::Latest, LogBound::Number(to))
            if *to < live_start =>
        {
            Err(RouteError::InvalidParams("fromBlock exceeds toBlock"))
        }
        (LogBound::Number(from), LogBound::Pending) if *from < live_start => {
            // Pending logs cannot be merged safely with a historical half-query because
            // the pending view can change between the two backend calls.
            Ok(LogsPlan::Archive)
        }
        (LogBound::Number(from), LogBound::Number(to))
            if *from < live_start && *to < live_start =>
        {
            Ok(LogsPlan::Archive)
        }
        (LogBound::Number(from), _) if *from < live_start => {
            let archive_to = live_start - 1;
            Ok(LogsPlan::Split { archive_to, live_from: live_start })
        }
        _ => Ok(LogsPlan::Live),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LogBound {
    Number(u64),
    Finalized,
    Safe,
    Latest,
    Pending,
}

fn parse_log_bound(value: &Value) -> Result<LogBound, RouteError> {
    let value =
        value.as_str().ok_or(RouteError::InvalidParams("log block bound must be a string"))?;
    match value {
        "earliest" => Ok(LogBound::Number(0)),
        "finalized" => Ok(LogBound::Finalized),
        "safe" => Ok(LogBound::Safe),
        "latest" => Ok(LogBound::Latest),
        "pending" => Ok(LogBound::Pending),
        _ => parse_quantity(value).map(LogBound::Number),
    }
}

const fn tag_order(bound: &LogBound) -> Option<u8> {
    match bound {
        LogBound::Number(_) => None,
        LogBound::Finalized => Some(0),
        LogBound::Safe => Some(1),
        LogBound::Latest => Some(2),
        LogBound::Pending => Some(3),
    }
}

fn replace_log_range(
    request: &Value,
    from: Option<u64>,
    to: Option<u64>,
    live_start: u64,
) -> Result<Value, RouterError> {
    let mut request = request.clone();
    let filter = request
        .get_mut("params")
        .and_then(Value::as_array_mut)
        .and_then(|params| params.first_mut())
        .and_then(Value::as_object_mut)
        .ok_or_else(|| RouterError::Configuration("validated log filter disappeared".to_owned()))?;
    if let Some(from) = from {
        filter.insert("fromBlock".to_owned(), Value::String(format!("0x{from:x}")));
    }
    if let Some(to) = to {
        filter.insert("toBlock".to_owned(), Value::String(format!("0x{to:x}")));
    }
    if from.is_none() {
        let original_from = filter.get("fromBlock").cloned();
        if original_from.as_ref().is_some_and(|value| {
            parse_log_bound(value).is_ok_and(
                |bound| matches!(bound, LogBound::Number(number) if number >= live_start),
            )
        }) {
            return Err(RouterError::Configuration(
                "archive log split received a live-only lower bound".to_owned(),
            ))
        }
    }
    Ok(request)
}

fn merge_log_responses(
    archive: Value,
    live: Value,
    archive_to: u64,
    live_from: u64,
) -> Result<Value, RouterError> {
    let expected_id = archive.get("id").ok_or_else(|| RouterError::Backend {
        backend: "archive",
        message: "response has no id".to_owned(),
    })?;
    validate_backend_response(&archive, expected_id).map_err(|message| RouterError::Backend {
        backend: "archive",
        message: message.to_owned(),
    })?;
    validate_backend_response(&live, expected_id)
        .map_err(|message| RouterError::Backend { backend: "live", message: message.to_owned() })?;
    let archive_object = archive.as_object().ok_or_else(|| RouterError::Backend {
        backend: "archive",
        message: "response is not an object".to_owned(),
    })?;
    let live_object = live.as_object().ok_or_else(|| RouterError::Backend {
        backend: "live",
        message: "response is not an object".to_owned(),
    })?;
    if let Some(error) = archive_object.get("error") {
        return Ok(json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": archive_object.get("id").cloned().unwrap_or(Value::Null),
            "error": error,
        }))
    }
    if let Some(error) = live_object.get("error") {
        return Ok(json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": live_object.get("id").cloned().unwrap_or(Value::Null),
            "error": error,
        }))
    }
    let archive_logs = archive_object.get("result").and_then(Value::as_array).ok_or_else(|| {
        RouterError::Backend {
            backend: "archive",
            message: "eth_getLogs result is not an array".to_owned(),
        }
    })?;
    let live_logs = live_object.get("result").and_then(Value::as_array).ok_or_else(|| {
        RouterError::Backend {
            backend: "live",
            message: "eth_getLogs result is not an array".to_owned(),
        }
    })?;
    validate_log_range(archive_logs, None, Some(archive_to), "archive")?;
    validate_log_range(live_logs, Some(live_from), None, "live")?;
    let mut logs = Vec::with_capacity(archive_logs.len().saturating_add(live_logs.len()));
    logs.extend(archive_logs.iter().cloned());
    logs.extend(live_logs.iter().cloned());
    Ok(json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": archive_object.get("id").cloned().unwrap_or(Value::Null),
        "result": logs,
    }))
}

fn validate_log_range(
    logs: &[Value],
    minimum: Option<u64>,
    maximum: Option<u64>,
    backend: &'static str,
) -> Result<(), RouterError> {
    for log in logs {
        let number = log
            .get("blockNumber")
            .and_then(Value::as_str)
            .ok_or_else(|| RouterError::Backend {
                backend,
                message: "log has no blockNumber".to_owned(),
            })
            .and_then(|value| {
                parse_quantity(value).map_err(|_| RouterError::Backend {
                    backend,
                    message: "log has an invalid blockNumber".to_owned(),
                })
            })?;
        if minimum.is_some_and(|minimum| number < minimum) ||
            maximum.is_some_and(|maximum| number > maximum)
        {
            return Err(RouterError::Backend {
                backend,
                message: "log falls outside its routed block range".to_owned(),
            })
        }
    }
    Ok(())
}

/// Pinned chain identity and retained-history values proved by the readiness endpoint.
#[derive(Clone, Copy, Debug)]
pub struct ReadinessConfig<'a> {
    /// Expected EVM chain ID for both backends.
    pub expected_chain_id: u64,
    /// Exact live-history anchor hash.
    pub anchor_hash: &'a str,
    /// Pre-Savanna block used for retained-history probes.
    pub history_probe_number: u64,
    /// Exact hash of the retained-history probe block.
    pub history_probe_hash: &'a str,
    /// Account whose balance is pinned at the retained-history probe block.
    pub history_probe_address: &'a str,
    /// Exact balance expected for `history_probe_address`.
    pub history_probe_balance: &'a str,
    /// Transaction whose receipt must belong to the retained-history probe block.
    pub history_probe_transaction_hash: &'a str,
    /// Contract whose storage is pinned at the retained-history probe block.
    pub history_storage_probe_address: &'a str,
    /// Storage key pinned for `history_storage_probe_address`.
    pub history_storage_probe_slot: &'a str,
    /// Exact 32-byte value expected at `history_storage_probe_slot`.
    pub history_storage_probe_value: &'a str,
    /// Maximum allowed height difference between the two backends.
    pub max_head_lag: u64,
}

/// Builds a JSON object used by the readiness endpoint.
pub async fn readiness(
    router: &RpcRouter,
    readiness: ReadinessConfig<'_>,
) -> Result<Value, RouterError> {
    let ReadinessConfig {
        expected_chain_id,
        anchor_hash,
        history_probe_number,
        history_probe_hash,
        history_probe_address,
        history_probe_balance,
        history_probe_transaction_hash,
        history_storage_probe_address,
        history_storage_probe_slot,
        history_storage_probe_value,
        max_head_lag,
    } = readiness;
    parse_hash(anchor_hash).map_err(|_| {
        RouterError::Configuration("configured anchor hash is not 32 bytes".to_owned())
    })?;
    parse_hash(history_probe_hash).map_err(|_| {
        RouterError::Configuration("configured history probe hash is not 32 bytes".to_owned())
    })?;
    parse_address(history_probe_address).map_err(|_| {
        RouterError::Configuration("configured history probe address is not 20 bytes".to_owned())
    })?;
    validate_unbounded_quantity(history_probe_balance).map_err(|_| {
        RouterError::Configuration(
            "configured history probe balance is not a canonical quantity".to_owned(),
        )
    })?;
    parse_hash(history_probe_transaction_hash).map_err(|_| {
        RouterError::Configuration(
            "configured history probe transaction hash is not 32 bytes".to_owned(),
        )
    })?;
    parse_address(history_storage_probe_address).map_err(|_| {
        RouterError::Configuration(
            "configured history storage probe address is not 20 bytes".to_owned(),
        )
    })?;
    validate_storage_key(history_storage_probe_slot).map_err(|_| {
        RouterError::Configuration("configured history storage probe slot is invalid".to_owned())
    })?;
    validate_storage_value(history_storage_probe_value).map_err(|_| {
        RouterError::Configuration(
            "configured history storage probe value is not 32 bytes".to_owned(),
        )
    })?;
    if history_probe_number >= router.config.live_history_start {
        return Err(RouterError::Configuration(
            "history probe must be below the live history boundary".to_owned(),
        ))
    }
    let budget = Arc::new(ResponseBudget::new(router.config.max_response_bytes));
    let history_probe_quantity = format!("0x{history_probe_number:x}");
    let chain_request = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": "chain",
        "method": "eth_chainId",
        "params": [],
    });
    let anchor_request = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": "anchor",
        "method": "eth_getBlockByNumber",
        "params": [format!("0x{:x}", router.config.live_history_start), false],
    });
    let history_request = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": "history",
        "method": "eth_getBlockByNumber",
        "params": [history_probe_quantity.clone(), false],
    });
    let history_balance_request = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": "history-balance",
        "method": "eth_getBalance",
        "params": [history_probe_address, history_probe_quantity.clone()],
    });
    let history_receipt_request = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": "history-receipt",
        "method": "eth_getTransactionReceipt",
        "params": [history_probe_transaction_hash],
    });
    let history_logs_request = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": "history-logs",
        "method": "eth_getLogs",
        "params": [{
            "fromBlock": history_probe_quantity.clone(),
            "toBlock": history_probe_quantity,
            "address": history_probe_address,
        }],
    });
    let history_hash_reference = json!({
        "blockHash": history_probe_hash,
        "requireCanonical": true,
    });
    let routed_history_balance_request = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": "routed-history-balance",
        "method": "eth_getBalance",
        "params": [history_probe_address, history_hash_reference.clone()],
    });
    let routed_history_storage_request = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": "routed-history-storage",
        "method": "eth_getStorageValues",
        "params": [{
            (history_storage_probe_address): [history_storage_probe_slot],
        }, history_hash_reference],
    });
    let head_request = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": "head",
        "method": "eth_blockNumber",
        "params": [],
    });
    let (
        live_chain,
        archive_chain,
        live_anchor,
        archive_anchor,
        archive_history,
        archive_history_balance,
        archive_history_receipt,
        archive_history_logs,
        routed_history_balance,
        routed_history_storage,
        live_head,
        archive_head,
    ) = tokio::join!(
        router.live.call(&chain_request, true, budget.as_ref()),
        router.archive.call(&chain_request, true, budget.as_ref()),
        router.live.call(&anchor_request, true, budget.as_ref()),
        router.archive.call(&anchor_request, true, budget.as_ref()),
        router.archive.call(&history_request, true, budget.as_ref()),
        router.archive.call(&history_balance_request, true, budget.as_ref()),
        router.archive.call(&history_receipt_request, true, budget.as_ref()),
        router.archive.call(&history_logs_request, true, budget.as_ref()),
        router.dispatch_with_budget(routed_history_balance_request, budget.clone()),
        router.dispatch_with_budget(routed_history_storage_request, budget.clone()),
        router.live.call(&head_request, true, budget.as_ref()),
        router.archive.call(&head_request, true, budget.as_ref()),
    );
    let live_chain = response_quantity(live_chain?, "live", "chain id")?;
    let archive_chain = response_quantity(archive_chain?, "archive", "chain id")?;
    if live_chain != expected_chain_id || archive_chain != expected_chain_id {
        return Err(RouterError::Backend {
            backend: "identity",
            message: "backend chain id does not match the configured chain".to_owned(),
        })
    }
    let live_anchor = response_block_hash(live_anchor?, "live", "anchor")?;
    let archive_anchor = response_block_hash(archive_anchor?, "archive", "anchor")?;
    if !live_anchor.eq_ignore_ascii_case(anchor_hash) ||
        !archive_anchor.eq_ignore_ascii_case(anchor_hash)
    {
        return Err(RouterError::Backend {
            backend: "identity",
            message: "backend anchor hash does not match the configured anchor".to_owned(),
        })
    }
    let archive_history = response_block_hash(archive_history?, "archive", "history probe")?;
    if !archive_history.eq_ignore_ascii_case(history_probe_hash) {
        return Err(RouterError::Backend {
            backend: "archive",
            message: "history probe hash does not match".to_owned(),
        })
    }
    response_expected_quantity(
        archive_history_balance?,
        "archive",
        "history balance",
        history_probe_balance,
    )?;
    response_receipt_identity(
        archive_history_receipt?,
        "archive",
        history_probe_transaction_hash,
        history_probe_hash,
        history_probe_number,
    )?;
    response_empty_array(archive_history_logs?, "archive", "history logs")?;
    response_expected_quantity(
        routed_history_balance,
        "router",
        "routed history balance",
        history_probe_balance,
    )?;
    response_expected_storage_value(
        routed_history_storage,
        "router",
        history_storage_probe_address,
        history_storage_probe_value,
    )?;
    let live_head = response_quantity(live_head?, "live", "head")?;
    let archive_head = response_quantity(archive_head?, "archive", "head")?;
    if live_head.abs_diff(archive_head) > max_head_lag {
        return Err(RouterError::Backend {
            backend: "identity",
            message: "backend heads exceed the configured lag".to_owned(),
        })
    }
    let common = live_head.min(archive_head);
    let common_request = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": "common",
        "method": "eth_getBlockByNumber",
        "params": [format!("0x{common:x}"), false],
    });
    let (live_common, archive_common) = tokio::join!(
        router.live.call(&common_request, true, budget.as_ref()),
        router.archive.call(&common_request, true, budget.as_ref())
    );
    let live_common = response_block_hash(live_common?, "live", "common head")?;
    let archive_common = response_block_hash(archive_common?, "archive", "common head")?;
    if !live_common.eq_ignore_ascii_case(&archive_common) {
        return Err(RouterError::Backend {
            backend: "identity",
            message: "backend hashes differ at their common head".to_owned(),
        })
    }
    Ok(json!({
        "ready": true,
        "chain_id": expected_chain_id,
        "live_history_start": router.config.live_history_start,
        "anchor_hash": anchor_hash,
        "history_probe_number": history_probe_number,
        "history_probe_hash": history_probe_hash,
        "history_probe_address": history_probe_address,
        "history_probe_balance": history_probe_balance,
        "history_probe_transaction_hash": history_probe_transaction_hash,
        "history_storage_probe_address": history_storage_probe_address,
        "history_storage_probe_slot": history_storage_probe_slot,
        "history_storage_probe_value": history_storage_probe_value,
        "live_head": live_head,
        "archive_head": archive_head,
        "common_head": common,
        "common_hash": live_common,
    }))
}

fn response_quantity(
    response: Option<Value>,
    backend: &'static str,
    field: &'static str,
) -> Result<u64, RouterError> {
    let response = response.ok_or_else(|| missing_response(backend))?;
    let value = response.get("result").and_then(Value::as_str).ok_or_else(|| {
        RouterError::Backend { backend, message: format!("{field} response is not a quantity") }
    })?;
    parse_quantity(value).map_err(|_| RouterError::Backend {
        backend,
        message: format!("{field} response is not a valid quantity"),
    })
}

fn response_expected_quantity(
    response: Option<Value>,
    backend: &'static str,
    field: &'static str,
    expected: &str,
) -> Result<(), RouterError> {
    let response = response.ok_or_else(|| missing_response(backend))?;
    let value = response.get("result").and_then(Value::as_str).ok_or_else(|| {
        RouterError::Backend { backend, message: format!("{field} response is not a quantity") }
    })?;
    validate_unbounded_quantity(value).map_err(|_| RouterError::Backend {
        backend,
        message: format!("{field} response is not a valid quantity"),
    })?;
    if !value.eq_ignore_ascii_case(expected) {
        return Err(RouterError::Backend {
            backend,
            message: format!("{field} does not match the configured value"),
        })
    }
    Ok(())
}

fn response_receipt_identity(
    response: Option<Value>,
    backend: &'static str,
    expected_transaction_hash: &str,
    expected_block_hash: &str,
    expected_block_number: u64,
) -> Result<(), RouterError> {
    let response = response.ok_or_else(|| missing_response(backend))?;
    let receipt =
        response.get("result").and_then(Value::as_object).ok_or_else(|| RouterError::Backend {
            backend,
            message: "history receipt response is not an object".to_owned(),
        })?;
    let transaction_hash =
        receipt.get("transactionHash").and_then(Value::as_str).ok_or_else(|| {
            RouterError::Backend {
                backend,
                message: "history receipt has no transaction hash".to_owned(),
            }
        })?;
    let block_hash = receipt.get("blockHash").and_then(Value::as_str).ok_or_else(|| {
        RouterError::Backend { backend, message: "history receipt has no block hash".to_owned() }
    })?;
    let block_number = receipt
        .get("blockNumber")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::Backend {
            backend,
            message: "history receipt has no block number".to_owned(),
        })
        .and_then(|value| {
            parse_quantity(value).map_err(|_| RouterError::Backend {
                backend,
                message: "history receipt has an invalid block number".to_owned(),
            })
        })?;
    parse_hash(transaction_hash).map_err(|_| RouterError::Backend {
        backend,
        message: "history receipt has an invalid transaction hash".to_owned(),
    })?;
    parse_hash(block_hash).map_err(|_| RouterError::Backend {
        backend,
        message: "history receipt has an invalid block hash".to_owned(),
    })?;
    let transaction_matches = transaction_hash.eq_ignore_ascii_case(expected_transaction_hash);
    let block_matches = block_hash.eq_ignore_ascii_case(expected_block_hash);
    if !(transaction_matches && block_matches && block_number == expected_block_number) {
        return Err(RouterError::Backend {
            backend,
            message: "history receipt identity does not match the configured probe".to_owned(),
        })
    }
    Ok(())
}

fn response_empty_array(
    response: Option<Value>,
    backend: &'static str,
    field: &'static str,
) -> Result<(), RouterError> {
    let response = response.ok_or_else(|| missing_response(backend))?;
    let result = response.get("result").and_then(Value::as_array).ok_or_else(|| {
        RouterError::Backend { backend, message: format!("{field} response is not an array") }
    })?;
    if !result.is_empty() {
        return Err(RouterError::Backend {
            backend,
            message: format!("{field} response is not empty"),
        })
    }
    Ok(())
}

fn response_expected_storage_value(
    response: Option<Value>,
    backend: &'static str,
    address: &str,
    expected: &str,
) -> Result<(), RouterError> {
    let response = response.ok_or_else(|| missing_response(backend))?;
    let result =
        response.get("result").and_then(Value::as_object).ok_or_else(|| RouterError::Backend {
            backend,
            message: "routed history storage response is not an object".to_owned(),
        })?;
    if result.len() != 1 {
        return Err(RouterError::Backend {
            backend,
            message: "routed history storage response has unexpected addresses".to_owned(),
        })
    }
    let values =
        result.get(&address.to_ascii_lowercase()).and_then(Value::as_array).ok_or_else(|| {
            RouterError::Backend {
                backend,
                message: "routed history storage response has no probe address".to_owned(),
            }
        })?;
    let value = values.as_slice();
    if value.len() != 1 ||
        !value[0].as_str().is_some_and(|value| value.eq_ignore_ascii_case(expected))
    {
        return Err(RouterError::Backend {
            backend,
            message: "routed history storage response does not contain the expected value"
                .to_owned(),
        })
    }
    Ok(())
}

fn response_block_hash(
    response: Option<Value>,
    backend: &'static str,
    field: &'static str,
) -> Result<String, RouterError> {
    let response = response.ok_or_else(|| missing_response(backend))?;
    let hash = response.pointer("/result/hash").and_then(Value::as_str).ok_or_else(|| {
        RouterError::Backend { backend, message: format!("{field} response has no block hash") }
    })?;
    parse_hash(hash).map_err(|_| RouterError::Backend {
        backend,
        message: format!("{field} response has an invalid block hash"),
    })?;
    Ok(hash.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{header::CONTENT_TYPE, Request, Response};
    use http_body_util::BodyExt;
    use jsonrpsee_server::{serve, HttpBody};
    use std::{convert::Infallible, future::Future, sync::Mutex};
    use tokio::{net::TcpListener, task::JoinHandle};
    use tower::service_fn;

    const LIVE_START: u64 = 1_000;

    fn params(values: Vec<Value>) -> Value {
        Value::Array(values)
    }

    async fn spawn_backend<F, Fut>(handler: F) -> (Url, JoinHandle<()>)
    where
        F: Fn(Value) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = Value> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let handler = handler.clone();
                let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                    let handler = handler.clone();
                    async move {
                        let bytes = request.into_body().collect().await.unwrap().to_bytes();
                        let request = serde_json::from_slice(&bytes).unwrap();
                        let response = handler(request).await;
                        Ok::<_, Infallible>(
                            Response::builder()
                                .header(CONTENT_TYPE, "application/json")
                                .body(HttpBody::from(serde_json::to_vec(&response).unwrap()))
                                .unwrap(),
                        )
                    }
                });
                tokio::spawn(async move {
                    serve(stream, service).await.unwrap();
                });
            }
        });
        (format!("http://{address}/").parse().unwrap(), task)
    }

    async fn spawn_optional_backend<F, Fut>(handler: F) -> (Url, JoinHandle<()>)
    where
        F: Fn(Value) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = Option<Value>> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let handler = handler.clone();
                let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                    let handler = handler.clone();
                    async move {
                        let bytes = request.into_body().collect().await.unwrap().to_bytes();
                        let request = serde_json::from_slice(&bytes).unwrap();
                        let response = match handler(request).await {
                            Some(response) => Response::builder()
                                .header(CONTENT_TYPE, "application/json")
                                .body(HttpBody::from(serde_json::to_vec(&response).unwrap()))
                                .unwrap(),
                            None => Response::builder()
                                .status(http::StatusCode::NO_CONTENT)
                                .body(HttpBody::empty())
                                .unwrap(),
                        };
                        Ok::<_, Infallible>(response)
                    }
                });
                tokio::spawn(async move {
                    serve(stream, service).await.unwrap();
                });
            }
        });
        (format!("http://{address}/").parse().unwrap(), task)
    }

    #[test]
    fn explicit_block_routes_across_history_boundary() {
        assert_eq!(
            route_plan(
                "eth_getBlockByNumber",
                &params(vec![json!("0x3e7"), json!(false)]),
                LIVE_START
            ),
            Ok(RoutePlan::Archive)
        );
        assert_eq!(
            route_plan(
                "eth_getBlockByNumber",
                &params(vec![json!("0x3e8"), json!(false)]),
                LIVE_START
            ),
            Ok(RoutePlan::Live)
        );
        assert_eq!(
            route_plan(
                "eth_getBalance",
                &params(vec![
                    json!("0x0000000000000000000000000000000000000000"),
                    json!("earliest")
                ]),
                LIVE_START
            ),
            Ok(RoutePlan::Archive)
        );
        assert_eq!(
            route_plan("eth_call", &params(vec![json!({}), json!("finalized")]), LIVE_START),
            Ok(RoutePlan::Live)
        );
    }

    #[test]
    fn hash_lookups_fall_back_but_live_methods_do_not() {
        assert_eq!(
            route_plan(
                "eth_getTransactionReceipt",
                &params(vec![json!(format!("0x{}", "11".repeat(32)))]),
                LIVE_START
            ),
            Ok(RoutePlan::LiveThenArchive)
        );
        assert_eq!(
            route_plan("eth_sendRawTransaction", &params(vec![json!("0x01")]), LIVE_START),
            Ok(RoutePlan::Live)
        );
    }

    #[test]
    fn state_hash_lookups_route_directly_to_archive() {
        let address = format!("0x{}", "11".repeat(20));
        let hash = format!("0x{}", "22".repeat(32));
        let block = json!({"blockHash": hash, "requireCanonical": true});
        for (method, method_params) in [
            ("eth_getBalance", params(vec![json!(address.clone()), block.clone()])),
            ("eth_getTransactionCount", params(vec![json!(address.clone()), block.clone()])),
            ("eth_getCode", params(vec![json!(address.clone()), block.clone()])),
            ("eth_call", params(vec![json!({}), block.clone()])),
            ("eth_estimateGas", params(vec![json!({}), block.clone()])),
            ("eth_getStorageAt", params(vec![json!(address), json!("0x0"), block.clone()])),
        ] {
            assert_eq!(
                route_plan(method, &method_params, LIVE_START),
                Ok(RoutePlan::Archive),
                "{method}",
            );
        }

        let storage_values = route_plan(
            "eth_getStorageValues",
            &params(vec![json!({(address): ["0x0"]}), block]),
            LIVE_START,
        )
        .unwrap();
        assert!(matches!(storage_values, RoutePlan::ArchiveStorageValues(_)));
    }

    #[test]
    fn router_surface_matches_telos_public_policy_and_incumbent_exceptions() {
        for method in [
            "eth_accounts",
            "eth_baseFee",
            "eth_blockNumber",
            "eth_chainId",
            "eth_gasPrice",
            "eth_maxPriorityFeePerGas",
            "eth_sendRawTransaction",
            "net_peerCount",
            "net_version",
            "web3_sha3",
        ] {
            assert_eq!(
                route_plan(method, &params(Vec::new()), LIVE_START),
                Ok(RoutePlan::Live),
                "{method}",
            );
        }
        assert_eq!(
            route_plan("eth_getHeaderByNumber", &params(vec![json!("0x3e8")]), LIVE_START),
            Ok(RoutePlan::Live),
        );

        for method in [
            "eth_getBlockByHash",
            "eth_getBlockTransactionCountByHash",
            "eth_getHeaderByHash",
            "eth_getRawTransactionByBlockHashAndIndex",
            "eth_getRawTransactionByHash",
            "eth_getTransactionByBlockHashAndIndex",
            "eth_getTransactionByHash",
            "eth_getTransactionReceipt",
            "eth_getUncleByBlockHashAndIndex",
            "eth_getUncleCountByBlockHash",
        ] {
            assert_eq!(
                route_plan(method, &params(Vec::new()), LIVE_START),
                Ok(RoutePlan::LiveThenArchive),
                "{method}",
            );
        }

        for method in [
            "eth_getBlockByNumber",
            "eth_getBlockReceipts",
            "eth_getBlockTransactionCountByNumber",
            "eth_getHeaderByNumber",
            "eth_getRawTransactionByBlockNumberAndIndex",
            "eth_getTransactionByBlockNumberAndIndex",
            "eth_getUncleByBlockNumberAndIndex",
            "eth_getUncleCountByBlockNumber",
        ] {
            assert_eq!(
                route_plan(method, &params(vec![json!("0x3e7")]), LIVE_START),
                Ok(RoutePlan::Archive),
                "{method}",
            );
        }

        for method in [
            "eth_call",
            "eth_estimateGas",
            "eth_getBalance",
            "eth_getCode",
            "eth_getTransactionCount",
        ] {
            assert_eq!(
                route_plan(method, &params(vec![json!({}), json!("0x3e7")]), LIVE_START),
                Ok(RoutePlan::Archive),
                "{method}",
            );
        }
        assert!(matches!(
            route_plan(
                "eth_getStorageValues",
                &params(vec![
                    json!({
                        "0x0000000000000000000000000000000000000000": ["0x0"],
                    }),
                    json!("0x3e7")
                ]),
                LIVE_START,
            ),
            Ok(RoutePlan::ArchiveStorageValues(_)),
        ));
        assert_eq!(
            route_plan(
                "eth_getStorageAt",
                &params(vec![json!("0x00"), json!("0x0"), json!("0x3e7")]),
                LIVE_START,
            ),
            Ok(RoutePlan::Archive),
        );
        assert_eq!(
            route_plan("eth_getStorageAt", &params(vec![json!("0x00"), json!("0x0")]), LIVE_START,),
            Ok(RoutePlan::Live),
        );
        assert_eq!(
            route_plan(
                "eth_getStorageValues",
                &params(vec![json!({
                    "0x0000000000000000000000000000000000000000": ["0x0"],
                })]),
                LIVE_START,
            ),
            Ok(RoutePlan::Live),
        );
        assert_eq!(
            route_plan("eth_getLogs", &params(vec![json!({})]), LIVE_START),
            Ok(RoutePlan::Logs(LogsPlan::Live)),
        );

        for method in [
            "eth_newFilter",
            "eth_newBlockFilter",
            "eth_newPendingTransactionFilter",
            "eth_getFilterChanges",
            "eth_getFilterLogs",
            "eth_uninstallFilter",
            "eth_feeHistory",
        ] {
            assert_eq!(
                route_plan(method, &params(Vec::new()), LIVE_START),
                Ok(RoutePlan::Archive),
                "{method}",
            );
        }
    }

    #[test]
    fn unsupported_and_replay_unsafe_methods_remain_fail_closed() {
        for method in [
            "eth_blobBaseFee",
            "eth_capabilities",
            "eth_coinbase",
            "eth_createAccessList",
            "eth_fillTransaction",
            "eth_getAccount",
            "eth_getAccountInfo",
            "eth_getProof",
            "eth_getTransactionBySenderAndNonce",
            "eth_getWork",
            "eth_hashrate",
            "eth_mining",
            "eth_pendingTransactions",
            "eth_protocolVersion",
            "eth_sendRawTransactionSync",
            "eth_sendTransaction",
            "eth_sign",
            "eth_signTransaction",
            "eth_signTypedData",
            "eth_submitHashrate",
            "eth_submitWork",
            "eth_subscribe",
            "eth_syncing",
            "eth_unsubscribe",
            "net_listening",
            "web3_clientVersion",
            "eth_callBundle",
            "eth_callMany",
            "eth_getBlockAccessList",
            "eth_getBlockAccessListByBlockHash",
            "eth_getBlockAccessListByBlockNumber",
            "eth_getBlockAccessListRaw",
            "eth_simulateV1",
            "mev_simBundle",
        ] {
            assert_eq!(
                route_plan(method, &params(Vec::new()), LIVE_START),
                Err(RouteError::MethodNotFound),
                "{method}",
            );
        }
    }

    #[test]
    fn log_ranges_are_nonoverlapping() {
        assert_eq!(
            route_plan(
                "eth_getLogs",
                &params(vec![json!({"fromBlock": "0x1", "toBlock": "latest"})]),
                LIVE_START
            ),
            Ok(RoutePlan::Logs(LogsPlan::Split {
                archive_to: LIVE_START - 1,
                live_from: LIVE_START,
            }))
        );
        assert_eq!(
            route_plan(
                "eth_getLogs",
                &params(vec![json!({"fromBlock": "0x3e8", "toBlock": "latest"})]),
                LIVE_START
            ),
            Ok(RoutePlan::Logs(LogsPlan::Live))
        );
        assert_eq!(
            route_plan(
                "eth_getLogs",
                &params(vec![json!({"fromBlock": "earliest", "toBlock": "0x3e7"})]),
                LIVE_START
            ),
            Ok(RoutePlan::Logs(LogsPlan::Archive))
        );
    }

    #[test]
    fn malformed_quantities_and_ambiguous_eip_1898_are_rejected() {
        assert!(route_plan(
            "eth_getBlockByNumber",
            &params(vec![json!("0x03"), json!(false)]),
            LIVE_START
        )
        .is_err());
        assert!(route_plan(
            "eth_getBalance",
            &params(vec![
                json!("0x0000000000000000000000000000000000000000"),
                json!({"blockNumber": "0x1", "blockHash": format!("0x{}", "22".repeat(32))})
            ]),
            LIVE_START
        )
        .is_err());
    }

    #[test]
    fn earliest_and_pending_log_ranges_route_without_cross_backend_pending_merges() {
        assert_eq!(parse_log_bound(&json!("earliest")), Ok(LogBound::Number(0)));
        assert_eq!(
            route_plan(
                "eth_getLogs",
                &params(vec![json!({"fromBlock": "earliest", "toBlock": "latest"})]),
                LIVE_START,
            ),
            Ok(RoutePlan::Logs(LogsPlan::Split {
                archive_to: LIVE_START - 1,
                live_from: LIVE_START,
            }))
        );
        assert_eq!(
            route_plan(
                "eth_getLogs",
                &params(vec![json!({"fromBlock": "earliest", "toBlock": "pending"})]),
                LIVE_START,
            ),
            Ok(RoutePlan::Logs(LogsPlan::Archive))
        );
        assert_eq!(
            route_plan(
                "eth_getLogs",
                &params(vec![json!({"fromBlock": "0x3e8", "toBlock": "pending"})]),
                LIVE_START,
            ),
            Ok(RoutePlan::Logs(LogsPlan::Live))
        );
        assert_eq!(
            route_plan(
                "eth_getLogs",
                &params(vec![json!({"fromBlock": "latest", "toBlock": "pending"})]),
                LIVE_START,
            ),
            Ok(RoutePlan::Logs(LogsPlan::Live))
        );
        assert!(route_plan(
            "eth_getLogs",
            &params(vec![json!({"fromBlock": "pending", "toBlock": "latest"})]),
            LIVE_START,
        )
        .is_err());
        assert!(route_plan(
            "eth_getLogs",
            &params(vec![json!({"fromBlock": "latest", "toBlock": "safe"})]),
            LIVE_START,
        )
        .is_err());
        assert!(route_plan(
            "eth_getLogs",
            &params(vec![json!({"fromBlock": "latest", "toBlock": "0x3e7"})]),
            LIVE_START,
        )
        .is_err());
    }

    #[test]
    fn configuration_rejects_zero_batches_and_identical_normalized_backends() {
        let live = BackendConfig { name: "live", url: "http://127.0.0.1:8545".parse().unwrap() };
        let archive =
            BackendConfig { name: "archive", url: "http://127.0.0.1:8545/".parse().unwrap() };
        let mut config = RouterConfig {
            live_history_start: LIVE_START,
            max_response_bytes: 1024,
            max_batch_len: 0,
            max_inflight: 2,
            backend_timeout: Duration::from_secs(1),
        };
        assert!(config.validate().is_err());
        config.max_batch_len = 8;
        config.max_response_bytes = encoded_json_len(&response_limit_error()) - 1;
        assert!(config.validate().is_err());
        config.max_response_bytes = 1024;
        assert!(RpcRouter::new(config, live, archive).is_err());
    }

    #[test]
    fn split_log_merge_rejects_backend_range_leakage() {
        let archive = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [{"blockNumber": "0x3e8"}],
        });
        let live = json!({"jsonrpc": "2.0", "id": 1, "result": []});
        assert!(merge_log_responses(archive, live, 999, 1_000).is_err());
    }

    #[test]
    fn backend_envelope_requires_a_well_formed_error() {
        assert_eq!(
            validate_backend_response(
                &json!({"jsonrpc": "2.0", "id": 1, "error": "not-an-object"}),
                &json!(1),
            ),
            Err("backend response error is not an object")
        );
        assert_eq!(
            validate_backend_response(
                &json!({"jsonrpc": "2.0", "id": 1, "error": {
                    "code": "-32000",
                    "message": "wrong code type",
                }}),
                &json!(1),
            ),
            Err("backend response error code is not an integer")
        );
        assert_eq!(
            validate_backend_response(
                &json!({"jsonrpc": "2.0", "id": 1, "error": {
                    "code": -32000,
                    "message": 7,
                }}),
                &json!(1),
            ),
            Err("backend response error message is not a string")
        );
    }

    #[tokio::test]
    async fn invalid_batch_and_unknown_method_fail_closed_without_backends() {
        let config = RouterConfig {
            live_history_start: LIVE_START,
            max_response_bytes: 1024,
            max_batch_len: 8,
            max_inflight: 2,
            backend_timeout: Duration::from_secs(1),
        };
        let router = RpcRouter::new(
            config,
            BackendConfig { name: "live", url: "http://127.0.0.1:1".parse().unwrap() },
            BackendConfig { name: "archive", url: "http://127.0.0.1:2".parse().unwrap() },
        )
        .unwrap();
        assert_eq!(
            router.dispatch(Value::Array(Vec::new())).await.unwrap()["error"]["code"],
            INVALID_REQUEST
        );
        let response = router
            .dispatch(json!({
                "jsonrpc": "2.0",
                "id": 9,
                "method": "debug_traceBlockByNumber",
                "params": ["0x1", {}],
            }))
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn malformed_no_id_requests_are_errors_but_valid_no_id_requests_are_notifications() {
        let router = RpcRouter::new(
            RouterConfig {
                live_history_start: LIVE_START,
                max_response_bytes: 1024,
                max_batch_len: 8,
                max_inflight: 2,
                backend_timeout: Duration::from_secs(1),
            },
            BackendConfig { name: "live", url: "http://127.0.0.1:1".parse().unwrap() },
            BackendConfig { name: "archive", url: "http://127.0.0.1:2".parse().unwrap() },
        )
        .unwrap();

        for malformed in [
            json!({"jsonrpc": "1.0", "method": "eth_chainId"}),
            json!({"jsonrpc": "2.0", "method": 7}),
            json!({"jsonrpc": "2.0", "id": true, "method": "eth_chainId"}),
        ] {
            let response = router.dispatch(malformed).await.unwrap();
            assert_eq!(response["id"], Value::Null);
            assert_eq!(response["error"]["code"], INVALID_REQUEST);
        }
        let response = router
            .dispatch(json!({"jsonrpc": "1.0", "id": 9, "method": "eth_chainId"}))
            .await
            .unwrap();
        assert_eq!(response["id"], 9);
        assert_eq!(response["error"]["code"], INVALID_REQUEST);

        assert!(router
            .dispatch(json!({
                "jsonrpc": "2.0",
                "method": "debug_traceBlockByNumber",
                "params": ["0x1", {}],
            }))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn mixed_batch_keeps_invalid_no_id_errors_and_omits_valid_notifications() {
        let router = RpcRouter::new(
            RouterConfig {
                live_history_start: LIVE_START,
                max_response_bytes: 1024,
                max_batch_len: 8,
                max_inflight: 2,
                backend_timeout: Duration::from_secs(1),
            },
            BackendConfig { name: "live", url: "http://127.0.0.1:1".parse().unwrap() },
            BackendConfig { name: "archive", url: "http://127.0.0.1:2".parse().unwrap() },
        )
        .unwrap();
        let response = router
            .dispatch(json!([
                {"jsonrpc": "1.0", "method": "eth_chainId"},
                {"jsonrpc": "2.0", "method": "debug_traceBlockByNumber", "params": []},
                {
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "debug_traceBlockByNumber",
                    "params": []
                }
            ]))
            .await
            .unwrap();
        let responses = response.as_array().unwrap();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["id"], Value::Null);
        assert_eq!(responses[0]["error"]["code"], INVALID_REQUEST);
        assert_eq!(responses[1]["id"], 3);
        assert_eq!(responses[1]["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn final_encoded_batch_response_never_exceeds_the_configured_bound() {
        let maximum = encoded_json_len(&response_limit_error());
        let router = RpcRouter::new(
            RouterConfig {
                live_history_start: LIVE_START,
                max_response_bytes: maximum,
                max_batch_len: 8,
                max_inflight: 2,
                backend_timeout: Duration::from_secs(1),
            },
            BackendConfig { name: "live", url: "http://127.0.0.1:1".parse().unwrap() },
            BackendConfig { name: "archive", url: "http://127.0.0.1:2".parse().unwrap() },
        )
        .unwrap();
        let response = router
            .dispatch(json!([
                {"jsonrpc": "2.0", "id": 1, "method": "debug_a", "params": []},
                {"jsonrpc": "2.0", "id": 2, "method": "debug_b", "params": []}
            ]))
            .await
            .unwrap();
        assert_eq!(response, response_limit_error());
        assert_eq!(encoded_json_len(&response), maximum);
    }

    #[tokio::test]
    async fn oversized_batch_is_rejected_before_any_backend_call() {
        let calls = Arc::new(Mutex::new(0usize));
        let (live_url, live_task) = spawn_backend({
            let calls = calls.clone();
            move |request: Value| {
                let calls = calls.clone();
                async move {
                    *calls.lock().unwrap() += 1;
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x28"})
                }
            }
        })
        .await;
        let router = RpcRouter::new(
            RouterConfig {
                live_history_start: LIVE_START,
                max_response_bytes: 1024,
                max_batch_len: 1,
                max_inflight: 2,
                backend_timeout: Duration::from_secs(1),
            },
            BackendConfig { name: "live", url: live_url },
            BackendConfig { name: "archive", url: "http://127.0.0.1:2".parse().unwrap() },
        )
        .unwrap();
        let response = router
            .dispatch(json!([
                {"jsonrpc": "2.0", "id": 1, "method": "eth_chainId", "params": []},
                {"jsonrpc": "2.0", "id": 2, "method": "eth_chainId", "params": []}
            ]))
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], INVALID_REQUEST);
        assert_eq!(*calls.lock().unwrap(), 0);
        live_task.abort();
    }

    #[tokio::test]
    async fn batch_backend_responses_share_one_aggregate_byte_budget() {
        let result = "x".repeat(128);
        let prototype = json!({"jsonrpc": "2.0", "id": 1, "result": result});
        let one_response_bytes = serde_json::to_vec(&prototype).unwrap().len();
        let (live_url, live_task) = spawn_backend(|request: Value| async move {
            json!({"jsonrpc": "2.0", "id": request["id"], "result": "x".repeat(128)})
        })
        .await;
        let router = RpcRouter::new(
            RouterConfig {
                live_history_start: LIVE_START,
                max_response_bytes: one_response_bytes + 1,
                max_batch_len: 2,
                max_inflight: 2,
                backend_timeout: Duration::from_secs(1),
            },
            BackendConfig { name: "live", url: live_url },
            BackendConfig { name: "archive", url: "http://127.0.0.1:2".parse().unwrap() },
        )
        .unwrap();
        let budget = ResponseBudget::new(one_response_bytes + 1);
        let first_request =
            json!({"jsonrpc": "2.0", "id": 1, "method": "eth_chainId", "params": []});
        let second_request =
            json!({"jsonrpc": "2.0", "id": 2, "method": "eth_chainId", "params": []});
        assert!(router.live.call(&first_request, true, &budget).await.unwrap().is_some());
        let error = router.live.call(&second_request, true, &budget).await.unwrap_err();
        assert!(error.to_string().contains("aggregate response exceeds"));

        let response = router
            .dispatch(json!([
                {"jsonrpc": "2.0", "id": 1, "method": "eth_chainId", "params": []},
                {"jsonrpc": "2.0", "id": 2, "method": "eth_chainId", "params": []}
            ]))
            .await
            .unwrap();
        assert_eq!(response, response_limit_error());
        assert!(encoded_json_len(&response) <= router.config.max_response_bytes);
        live_task.abort();
    }

    #[tokio::test]
    async fn split_log_backends_share_one_aggregate_byte_budget() {
        let prototype = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [{"blockNumber": "0x3e7", "data": "0x".to_owned() + &"ab".repeat(64)}],
        });
        let one_response_bytes = serde_json::to_vec(&prototype).unwrap().len();
        let (live_url, live_task) = spawn_backend(|request: Value| async move {
            json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": [{
                    "blockNumber": request["params"][0]["fromBlock"],
                    "data": "0x".to_owned() + &"ab".repeat(64),
                }],
            })
        })
        .await;
        let (archive_url, archive_task) = spawn_backend(|request: Value| async move {
            json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": [{
                    "blockNumber": request["params"][0]["toBlock"],
                    "data": "0x".to_owned() + &"ab".repeat(64),
                }],
            })
        })
        .await;
        let router = RpcRouter::new(
            RouterConfig {
                live_history_start: LIVE_START,
                max_response_bytes: one_response_bytes + 1,
                max_batch_len: 8,
                max_inflight: 2,
                backend_timeout: Duration::from_secs(1),
            },
            BackendConfig { name: "live", url: live_url },
            BackendConfig { name: "archive", url: archive_url },
        )
        .unwrap();
        let response = router
            .dispatch(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_getLogs",
                "params": [{"fromBlock": "0x1", "toBlock": "0x3e9"}],
            }))
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], ROUTER_ERROR);
        assert!(response["error"]["message"].as_str().unwrap().contains("aggregate response"));
        assert!(encoded_json_len(&response) <= router.config.max_response_bytes);
        live_task.abort();
        archive_task.abort();
    }

    #[tokio::test]
    async fn wrong_id_live_null_does_not_trigger_archive_fallback() {
        let archive_calls = Arc::new(Mutex::new(0usize));
        let (live_url, live_task) = spawn_backend(|_request: Value| async move {
            json!({"jsonrpc": "2.0", "id": 999, "result": null})
        })
        .await;
        let (archive_url, archive_task) = spawn_backend({
            let calls = archive_calls.clone();
            move |request: Value| {
                let calls = calls.clone();
                async move {
                    *calls.lock().unwrap() += 1;
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": {"unexpected": true}})
                }
            }
        })
        .await;
        let router = RpcRouter::new(
            RouterConfig {
                live_history_start: LIVE_START,
                max_response_bytes: 4096,
                max_batch_len: 8,
                max_inflight: 2,
                backend_timeout: Duration::from_secs(1),
            },
            BackendConfig { name: "live", url: live_url },
            BackendConfig { name: "archive", url: archive_url },
        )
        .unwrap();
        let response = router
            .dispatch(json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "eth_getTransactionReceipt",
                "params": [format!("0x{}", "11".repeat(32))],
            }))
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], ROUTER_ERROR);
        assert_eq!(*archive_calls.lock().unwrap(), 0);
        live_task.abort();
        archive_task.abort();
    }

    #[tokio::test]
    async fn backend_call_timeout_includes_waiting_for_the_shared_permit() {
        let (live_url, live_task) = spawn_backend(|request: Value| async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x28"})
        })
        .await;
        let router = RpcRouter::new(
            RouterConfig {
                live_history_start: LIVE_START,
                max_response_bytes: 4096,
                max_batch_len: 2,
                max_inflight: 1,
                backend_timeout: Duration::from_millis(400),
            },
            BackendConfig { name: "live", url: live_url },
            BackendConfig { name: "archive", url: "http://127.0.0.1:2".parse().unwrap() },
        )
        .unwrap();
        let response = router
            .dispatch(json!([
                {"jsonrpc": "2.0", "id": 1, "method": "eth_chainId", "params": []},
                {"jsonrpc": "2.0", "id": 2, "method": "eth_chainId", "params": []}
            ]))
            .await
            .unwrap();
        let responses = response.as_array().unwrap();
        assert_eq!(
            responses
                .iter()
                .filter(|response| response.get("result") == Some(&json!("0x28")))
                .count(),
            1
        );
        assert_eq!(
            responses
                .iter()
                .filter(|response| response.pointer("/error/code") == Some(&json!(ROUTER_ERROR)))
                .count(),
            1
        );
        live_task.abort();
    }

    #[tokio::test]
    async fn backend_routing_fallback_and_split_logs_are_end_to_end() {
        let live_calls = Arc::new(Mutex::new(Vec::new()));
        let archive_calls = Arc::new(Mutex::new(Vec::new()));
        let (live_url, live_task) = spawn_backend({
            let calls = live_calls.clone();
            move |request: Value| {
                let calls = calls.clone();
                async move {
                    calls.lock().unwrap().push(request.clone());
                    let id = request["id"].clone();
                    match request["method"].as_str().unwrap() {
                        "eth_getTransactionReceipt" => {
                            json!({"jsonrpc": "2.0", "id": id, "result": null})
                        }
                        "eth_getLogs" => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": [{
                                "blockNumber": request["params"][0]["fromBlock"],
                                "transactionIndex": "0x0",
                                "logIndex": "0x0",
                            }],
                        }),
                        _ => json!({"jsonrpc": "2.0", "id": id, "result": {"backend": "live"}}),
                    }
                }
            }
        })
        .await;
        let (archive_url, archive_task) = spawn_backend({
            let calls = archive_calls.clone();
            move |request: Value| {
                let calls = calls.clone();
                async move {
                    calls.lock().unwrap().push(request.clone());
                    let id = request["id"].clone();
                    match request["method"].as_str().unwrap() {
                        "eth_getLogs" => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": [{
                                "blockNumber": request["params"][0]["toBlock"],
                                "transactionIndex": "0x0",
                                "logIndex": "0x0",
                            }],
                        }),
                        _ => {
                            json!({"jsonrpc": "2.0", "id": id, "result": {"backend": "archive"}})
                        }
                    }
                }
            }
        })
        .await;
        let router = RpcRouter::new(
            RouterConfig {
                live_history_start: LIVE_START,
                max_response_bytes: 64 * 1024,
                max_batch_len: 8,
                max_inflight: 8,
                backend_timeout: Duration::from_secs(2),
            },
            BackendConfig { name: "live", url: live_url },
            BackendConfig { name: "archive", url: archive_url },
        )
        .unwrap();

        let old_block = router
            .dispatch(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_getBlockByNumber",
                "params": ["0x3e7", false],
            }))
            .await
            .unwrap();
        assert_eq!(old_block["result"]["backend"], "archive");

        let old_receipt = router
            .dispatch(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "eth_getTransactionReceipt",
                "params": [format!("0x{}", "11".repeat(32))],
            }))
            .await
            .unwrap();
        assert_eq!(old_receipt["result"]["backend"], "archive");

        let logs = router
            .dispatch(json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "eth_getLogs",
                "params": [{"fromBlock": "0x1", "toBlock": "0x3e9"}],
            }))
            .await
            .unwrap();
        assert_eq!(logs["result"][0]["blockNumber"], "0x3e7");
        assert_eq!(logs["result"][1]["blockNumber"], "0x3e8");

        let live_calls = live_calls.lock().unwrap();
        let archive_calls = archive_calls.lock().unwrap();
        assert_eq!(
            live_calls.iter().filter(|request| request["method"] == "eth_getLogs").count(),
            1
        );
        assert_eq!(
            archive_calls.iter().filter(|request| request["method"] == "eth_getLogs").count(),
            1
        );
        drop(live_calls);
        drop(archive_calls);
        live_task.abort();
        archive_task.abort();
    }

    #[tokio::test]
    async fn old_hash_state_request_uses_archive_without_touching_live() {
        let live_calls = Arc::new(Mutex::new(0usize));
        let archive_calls = Arc::new(Mutex::new(Vec::new()));
        let (live_url, live_task) = spawn_backend({
            let calls = live_calls.clone();
            move |request: Value| {
                let calls = calls.clone();
                async move {
                    *calls.lock().unwrap() += 1;
                    json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "error": {"code": -32001, "message": "block not found"},
                    })
                }
            }
        })
        .await;
        let (archive_url, archive_task) = spawn_backend({
            let calls = archive_calls.clone();
            move |request: Value| {
                let calls = calls.clone();
                async move {
                    calls.lock().unwrap().push(request.clone());
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x2a"})
                }
            }
        })
        .await;
        let router = RpcRouter::new(
            RouterConfig {
                live_history_start: LIVE_START,
                max_response_bytes: 64 * 1024,
                max_batch_len: 8,
                max_inflight: 8,
                backend_timeout: Duration::from_secs(2),
            },
            BackendConfig { name: "live", url: live_url },
            BackendConfig { name: "archive", url: archive_url },
        )
        .unwrap();
        let block_hash = format!("0x{}", "44".repeat(32));
        let response = router
            .dispatch(json!({
                "jsonrpc": "2.0",
                "id": 41,
                "method": "eth_getBalance",
                "params": [
                    format!("0x{}", "33".repeat(20)),
                    {"blockHash": block_hash, "requireCanonical": true},
                ],
            }))
            .await
            .unwrap();

        assert_eq!(response["id"], 41);
        assert_eq!(response["result"], "0x2a");
        assert_eq!(*live_calls.lock().unwrap(), 0);
        let archive_calls = archive_calls.lock().unwrap();
        assert_eq!(archive_calls.len(), 1);
        assert_eq!(archive_calls[0]["method"], "eth_getBalance");
        assert_eq!(archive_calls[0]["params"][1]["blockHash"], block_hash);
        live_task.abort();
        archive_task.abort();
    }

    #[tokio::test]
    async fn old_storage_values_are_synthesized_with_order_and_block_preserved() {
        let live_calls = Arc::new(Mutex::new(0usize));
        let archive_calls = Arc::new(Mutex::new(Vec::new()));
        let (live_url, live_task) = spawn_backend({
            let calls = live_calls.clone();
            move |request: Value| {
                let calls = calls.clone();
                async move {
                    *calls.lock().unwrap() += 1;
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": null})
                }
            }
        })
        .await;
        let (archive_url, archive_task) = spawn_backend({
            let calls = archive_calls.clone();
            move |request: Value| {
                let calls = calls.clone();
                async move {
                    calls.lock().unwrap().push(request.clone());
                    let slot = request["params"][1].as_str().unwrap();
                    let value = match slot {
                        "0x1" => format!("0x{:064x}", 1),
                        "0x2" => format!("0x{:064x}", 2),
                        "0x3" => format!("0x{:064x}", 3),
                        unexpected => panic!("unexpected storage slot: {unexpected}"),
                    };
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": value})
                }
            }
        })
        .await;
        let router = RpcRouter::new(
            RouterConfig {
                live_history_start: LIVE_START,
                max_response_bytes: 64 * 1024,
                max_batch_len: 8,
                max_inflight: 2,
                backend_timeout: Duration::from_secs(2),
            },
            BackendConfig { name: "live", url: live_url },
            BackendConfig { name: "archive", url: archive_url },
        )
        .unwrap();
        let address_a = format!("0x{}", "AA".repeat(20));
        let address_b = format!("0x{}", "bb".repeat(20));
        let response = router
            .dispatch(json!({
                "jsonrpc": "2.0",
                "id": 42,
                "method": "eth_getStorageValues",
                "params": [{
                    (address_a.clone()): ["0x2", "0x1"],
                    (address_b.clone()): ["0x3"],
                }, "0x3e7"],
            }))
            .await
            .unwrap();

        assert_eq!(response["id"], 42);
        assert_eq!(
            response["result"][address_a.to_ascii_lowercase()],
            json!([format!("0x{:064x}", 2), format!("0x{:064x}", 1)])
        );
        assert_eq!(
            response["result"][address_b.to_ascii_lowercase()],
            json!([format!("0x{:064x}", 3)])
        );
        assert_eq!(*live_calls.lock().unwrap(), 0);
        let archive_calls = archive_calls.lock().unwrap();
        assert_eq!(archive_calls.len(), 3);
        assert!(archive_calls.iter().all(|request| request["method"] == "eth_getStorageAt"));
        assert!(archive_calls.iter().all(|request| request["params"][2] == "0x3e7"));
        assert!(archive_calls.iter().all(|request| {
            request["params"][0]
                .as_str()
                .is_some_and(|address| address == address.to_ascii_lowercase())
        }));
        live_task.abort();
        archive_task.abort();
    }

    #[tokio::test]
    async fn empty_storage_maps_probe_the_requested_archive_block() {
        let live_calls = Arc::new(Mutex::new(0usize));
        let archive_calls = Arc::new(Mutex::new(Vec::new()));
        let known_hash = format!("0x{}", "77".repeat(32));
        let unknown_hash = format!("0x{}", "88".repeat(32));
        let (live_url, live_task) = spawn_backend({
            let calls = live_calls.clone();
            move |request: Value| {
                let calls = calls.clone();
                async move {
                    *calls.lock().unwrap() += 1;
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": null})
                }
            }
        })
        .await;
        let (archive_url, archive_task) = spawn_backend({
            let calls = archive_calls.clone();
            let known_hash = known_hash.clone();
            move |request: Value| {
                let calls = calls.clone();
                let known_hash = known_hash.clone();
                async move {
                    calls.lock().unwrap().push(request.clone());
                    if request.pointer("/params/1/blockHash").and_then(Value::as_str) ==
                        Some(known_hash.as_str())
                    {
                        json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x0"})
                    } else {
                        json!({
                            "jsonrpc": "2.0",
                            "id": request["id"],
                            "error": {"code": -32001, "message": "unknown block"},
                        })
                    }
                }
            }
        })
        .await;
        let router = RpcRouter::new(
            RouterConfig {
                live_history_start: LIVE_START,
                max_response_bytes: 64 * 1024,
                max_batch_len: 8,
                max_inflight: 2,
                backend_timeout: Duration::from_secs(2),
            },
            BackendConfig { name: "live", url: live_url },
            BackendConfig { name: "archive", url: archive_url },
        )
        .unwrap();
        let address_a = format!("0x{}", "aa".repeat(20));
        let address_b = format!("0x{}", "bb".repeat(20));
        let response = router
            .dispatch(json!({
                "jsonrpc": "2.0",
                "id": 45,
                "method": "eth_getStorageValues",
                "params": [{
                    (address_a.clone()): [],
                    (address_b.clone()): [],
                }, {"blockHash": known_hash, "requireCanonical": true}],
            }))
            .await
            .unwrap();
        assert_eq!(response["result"][address_a], json!([]));
        assert_eq!(response["result"][address_b], json!([]));

        let unknown = router
            .dispatch(json!({
                "jsonrpc": "2.0",
                "id": "unknown-empty-block",
                "method": "eth_getStorageValues",
                "params": [{
                    (format!("0x{}", "cc".repeat(20))): [],
                }, {"blockHash": unknown_hash, "requireCanonical": true}],
            }))
            .await
            .unwrap();
        assert_eq!(unknown["id"], "unknown-empty-block");
        assert_eq!(unknown["error"]["code"], -32001);
        assert_eq!(unknown["error"]["message"], "unknown block");
        assert_eq!(*live_calls.lock().unwrap(), 0);
        let archive_calls = archive_calls.lock().unwrap();
        assert_eq!(archive_calls.len(), 2);
        assert!(archive_calls.iter().all(|request| request["method"] == "eth_getBalance"));
        assert!(archive_calls
            .iter()
            .all(|request| request["id"] == "telos-router-storage-empty-probe"));
        live_task.abort();
        archive_task.abort();
    }

    #[tokio::test]
    async fn storage_fanout_is_capped_across_a_sixty_four_element_batch() {
        let live_calls = Arc::new(Mutex::new(0usize));
        let archive_calls = Arc::new(Mutex::new(0usize));
        let (live_url, live_task) = spawn_backend({
            let calls = live_calls.clone();
            move |request: Value| {
                let calls = calls.clone();
                async move {
                    *calls.lock().unwrap() += 1;
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": null})
                }
            }
        })
        .await;
        let (archive_url, archive_task) = spawn_backend({
            let calls = archive_calls.clone();
            move |request: Value| {
                let calls = calls.clone();
                async move {
                    *calls.lock().unwrap() += 1;
                    json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": format!("0x{}", "00".repeat(32)),
                    })
                }
            }
        })
        .await;
        let router = RpcRouter::new(
            RouterConfig {
                live_history_start: LIVE_START,
                max_response_bytes: 1024 * 1024,
                max_batch_len: 64,
                max_inflight: 64,
                backend_timeout: Duration::from_secs(30),
            },
            BackendConfig { name: "live", url: live_url },
            BackendConfig { name: "archive", url: archive_url },
        )
        .unwrap();
        let requests = (0..64)
            .map(|index| {
                let address = format!("0x{index:040x}");
                let slots = (0..17).map(|slot| format!("0x{slot:x}")).collect::<Vec<_>>();
                json!({
                    "jsonrpc": "2.0",
                    "id": index,
                    "method": "eth_getStorageValues",
                    "params": [{(address): slots}, "0x3e7"],
                })
            })
            .collect::<Vec<_>>();
        let response = router.dispatch(Value::Array(requests)).await.unwrap();
        let responses = response.as_array().unwrap();
        let successes =
            responses.iter().filter(|response| response.get("result").is_some()).count();
        let rejected = responses
            .iter()
            .filter(|response| {
                response
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .is_some_and(|message| message.contains("fan-out exceeds 1024"))
            })
            .count();

        assert_eq!(responses.len(), 64);
        assert_eq!(successes, 60);
        assert_eq!(rejected, 4);
        assert_eq!(*archive_calls.lock().unwrap(), successes * 17);
        assert!(*archive_calls.lock().unwrap() <= MAX_ARCHIVE_STORAGE_CALLS_PER_DISPATCH);
        assert_eq!(*live_calls.lock().unwrap(), 0);
        live_task.abort();
        archive_task.abort();
    }

    #[test]
    fn storage_values_validation_rejects_limits_and_ambiguous_addresses() {
        let address = format!("0x{}", "aa".repeat(20));
        let upper_address = address.to_ascii_uppercase().replacen("0X", "0x", 1);
        let block = json!("0x3e7");
        let duplicate = params(vec![
            json!({
                (address.clone()): ["0x0"],
                (upper_address): ["0x1"],
            }),
            block.clone(),
        ]);
        assert!(route_plan("eth_getStorageValues", &duplicate, LIVE_START).is_err());

        let malformed = params(vec![json!({"not-an-address": ["0x0"]}), block.clone()]);
        assert!(route_plan("eth_getStorageValues", &malformed, LIVE_START).is_err());
        let invalid_slot = params(vec![json!({(address.clone()): [7]}), block.clone()]);
        assert!(route_plan("eth_getStorageValues", &invalid_slot, LIVE_START).is_err());
        let too_long_key = format!("0x{}", "1".repeat(65));
        for invalid_key in ["", "0x", "1", "0xgg", too_long_key.as_str()] {
            let invalid_slot =
                params(vec![json!({(address.clone()): [invalid_key]}), block.clone()]);
            assert!(route_plan("eth_getStorageValues", &invalid_slot, LIVE_START).is_err());
        }
        let invalid_live_slot = params(vec![json!({(address.clone()): ["0x"]}), json!("latest")]);
        assert!(route_plan("eth_getStorageValues", &invalid_live_slot, LIVE_START).is_err());
        let maximum_key = format!("0x{}", "f".repeat(64));
        let valid_slot = params(vec![json!({(address.clone()): [maximum_key]}), block.clone()]);
        assert!(matches!(
            route_plan("eth_getStorageValues", &valid_slot, LIVE_START),
            Ok(RoutePlan::ArchiveStorageValues(_)),
        ));
        let empty = params(vec![json!({}), block.clone()]);
        assert!(route_plan("eth_getStorageValues", &empty, LIVE_START).is_err());
        let excessive_addresses = (0..=MAX_STORAGE_VALUES_SLOTS)
            .map(|index| (format!("0x{index:040x}"), Value::Array(Vec::new())))
            .collect::<Map<_, _>>();
        let excessive_addresses = params(vec![Value::Object(excessive_addresses), block.clone()]);
        assert!(route_plan("eth_getStorageValues", &excessive_addresses, LIVE_START).is_err());

        let maximum = vec![json!("0x0"); MAX_STORAGE_VALUES_SLOTS];
        let accepted = params(vec![json!({(address.clone()): maximum}), block.clone()]);
        assert!(matches!(
            route_plan("eth_getStorageValues", &accepted, LIVE_START),
            Ok(RoutePlan::ArchiveStorageValues(_)),
        ));
        let excessive = vec![json!("0x0"); MAX_STORAGE_VALUES_SLOTS + 1];
        let rejected = params(vec![json!({(address): excessive}), block]);
        assert!(route_plan("eth_getStorageValues", &rejected, LIVE_START).is_err());
    }

    #[tokio::test]
    async fn storage_values_rewrites_backend_error_id_and_rejects_invalid_values() {
        let live_calls = Arc::new(Mutex::new(0usize));
        let (live_url, live_task) = spawn_backend({
            let calls = live_calls.clone();
            move |request: Value| {
                let calls = calls.clone();
                async move {
                    *calls.lock().unwrap() += 1;
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": null})
                }
            }
        })
        .await;
        let (archive_url, archive_task) = spawn_backend(|request: Value| async move {
            match request["params"][1].as_str().unwrap() {
                "0x1" => json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": {
                        "code": -32042,
                        "message": "historical state unavailable",
                        "data": {"source": "incumbent"},
                    },
                }),
                "0x2" => {
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x1"})
                }
                "0x3" => json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": {
                        "code": "-32042",
                        "message": "code has the wrong type",
                    },
                }),
                unexpected => panic!("unexpected slot: {unexpected}"),
            }
        })
        .await;
        let router = RpcRouter::new(
            RouterConfig {
                live_history_start: LIVE_START,
                max_response_bytes: 64 * 1024,
                max_batch_len: 8,
                max_inflight: 2,
                backend_timeout: Duration::from_secs(2),
            },
            BackendConfig { name: "live", url: live_url },
            BackendConfig { name: "archive", url: archive_url },
        )
        .unwrap();
        let address = format!("0x{}", "55".repeat(20));
        let error = router
            .dispatch(json!({
                "jsonrpc": "2.0",
                "id": "original-error-id",
                "method": "eth_getStorageValues",
                "params": [{(address.clone()): ["0x1"]}, "0x3e7"],
            }))
            .await
            .unwrap();
        assert_eq!(error["id"], "original-error-id");
        assert_eq!(error["error"]["code"], -32042);
        assert_eq!(error["error"]["data"]["source"], "incumbent");

        let invalid = router
            .dispatch(json!({
                "jsonrpc": "2.0",
                "id": 43,
                "method": "eth_getStorageValues",
                "params": [{(address.clone()): ["0x2"]}, "0x3e7"],
            }))
            .await
            .unwrap();
        assert_eq!(invalid["id"], 43);
        assert_eq!(invalid["error"]["code"], ROUTER_ERROR);
        assert!(invalid["error"]["message"].as_str().unwrap().contains("exactly 32 bytes"));

        let malformed_error = router
            .dispatch(json!({
                "jsonrpc": "2.0",
                "id": 44,
                "method": "eth_getStorageValues",
                "params": [{(address): ["0x3"]}, "0x3e7"],
            }))
            .await
            .unwrap();
        assert_eq!(malformed_error["id"], 44);
        assert_eq!(malformed_error["error"]["code"], ROUTER_ERROR);
        assert!(malformed_error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("error code is not an integer"));
        assert_eq!(*live_calls.lock().unwrap(), 0);
        live_task.abort();
        archive_task.abort();
    }

    #[tokio::test]
    async fn storage_values_notification_fans_out_without_a_response() {
        let live_calls = Arc::new(Mutex::new(0usize));
        let archive_calls = Arc::new(Mutex::new(Vec::new()));
        let (live_url, live_task) = spawn_optional_backend({
            let calls = live_calls.clone();
            move |_request: Value| {
                let calls = calls.clone();
                async move {
                    *calls.lock().unwrap() += 1;
                    None
                }
            }
        })
        .await;
        let (archive_url, archive_task) = spawn_optional_backend({
            let calls = archive_calls.clone();
            move |request: Value| {
                let calls = calls.clone();
                async move {
                    calls.lock().unwrap().push(request);
                    None
                }
            }
        })
        .await;
        let router = RpcRouter::new(
            RouterConfig {
                live_history_start: LIVE_START,
                max_response_bytes: 64 * 1024,
                max_batch_len: 8,
                max_inflight: 2,
                backend_timeout: Duration::from_secs(2),
            },
            BackendConfig { name: "live", url: live_url },
            BackendConfig { name: "archive", url: archive_url },
        )
        .unwrap();
        let address = format!("0x{}", "66".repeat(20));
        let response = router
            .dispatch(json!({
                "jsonrpc": "2.0",
                "method": "eth_getStorageValues",
                "params": [{(address.clone()): ["0x1", "0x2"]}, "0x3e7"],
            }))
            .await;
        let empty_response = router
            .dispatch(json!({
                "jsonrpc": "2.0",
                "method": "eth_getStorageValues",
                "params": [{(address): []}, "0x3e7"],
            }))
            .await;

        assert!(response.is_none());
        assert!(empty_response.is_none());
        assert_eq!(*live_calls.lock().unwrap(), 0);
        let archive_calls = archive_calls.lock().unwrap();
        assert_eq!(archive_calls.len(), 3);
        assert!(archive_calls.iter().all(|request| request.get("id").is_none()));
        assert_eq!(
            archive_calls.iter().filter(|request| request["method"] == "eth_getStorageAt").count(),
            2
        );
        assert_eq!(
            archive_calls.iter().filter(|request| request["method"] == "eth_getBalance").count(),
            1
        );
        assert!(archive_calls
            .iter()
            .filter(|request| request["method"] == "eth_getStorageAt")
            .all(|request| request["params"][2] == "0x3e7"));
        assert!(archive_calls
            .iter()
            .filter(|request| request["method"] == "eth_getBalance")
            .all(|request| request["params"][1] == "0x3e7"));
        live_task.abort();
        archive_task.abort();
    }

    #[tokio::test]
    async fn readiness_proves_pre_savanna_balance_receipt_and_empty_logs() {
        let anchor_hash = format!("0x{}", "aa".repeat(32));
        let history_hash = format!("0x{}", "bb".repeat(32));
        let transaction_hash = format!("0x{}", "cc".repeat(32));
        let address = format!("0x{}", "dd".repeat(20));
        let storage_address = format!("0x{}", "12".repeat(20));
        let storage_value = format!("0x{}", "00".repeat(31) + "12");
        let common_hash = format!("0x{}", "ee".repeat(32));
        let balance = "0x123456789abcdef";

        let (live_url, live_task) = spawn_backend({
            let anchor_hash = anchor_hash.clone();
            let common_hash = common_hash.clone();
            move |request: Value| {
                let anchor_hash = anchor_hash.clone();
                let common_hash = common_hash.clone();
                async move {
                    let id = request["id"].clone();
                    match id.as_str().unwrap() {
                        "chain" => json!({"jsonrpc": "2.0", "id": id, "result": "0x28"}),
                        "anchor" => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"hash": anchor_hash},
                        }),
                        "head" => json!({"jsonrpc": "2.0", "id": id, "result": "0x3ec"}),
                        "common" => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"hash": common_hash},
                        }),
                        unexpected => panic!("unexpected live readiness request: {unexpected}"),
                    }
                }
            }
        })
        .await;
        let (archive_url, archive_task) = spawn_backend({
            let anchor_hash = anchor_hash.clone();
            let history_hash = history_hash.clone();
            let transaction_hash = transaction_hash.clone();
            let storage_address = storage_address.clone();
            let storage_value = storage_value.clone();
            let common_hash = common_hash.clone();
            move |request: Value| {
                let anchor_hash = anchor_hash.clone();
                let history_hash = history_hash.clone();
                let transaction_hash = transaction_hash.clone();
                let storage_address = storage_address.clone();
                let storage_value = storage_value.clone();
                let common_hash = common_hash.clone();
                async move {
                    let id = request["id"].clone();
                    match id.as_str().unwrap() {
                        "chain" => json!({"jsonrpc": "2.0", "id": id, "result": "0x28"}),
                        "anchor" => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"hash": anchor_hash},
                        }),
                        "history" => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"hash": history_hash},
                        }),
                        "history-balance" => {
                            json!({"jsonrpc": "2.0", "id": id, "result": balance})
                        }
                        "routed-history-balance" => {
                            assert_eq!(
                                request["params"][1],
                                json!({
                                    "blockHash": history_hash,
                                    "requireCanonical": true,
                                })
                            );
                            json!({"jsonrpc": "2.0", "id": id, "result": balance})
                        }
                        "history-receipt" => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "transactionHash": transaction_hash,
                                "blockHash": history_hash,
                                "blockNumber": "0x3e7",
                            },
                        }),
                        "history-logs" => {
                            json!({"jsonrpc": "2.0", "id": id, "result": []})
                        }
                        "telos-router-storage-0" => {
                            assert_eq!(request["method"], "eth_getStorageAt");
                            assert_eq!(request["params"][0], storage_address);
                            assert_eq!(request["params"][1], "0x2");
                            assert_eq!(
                                request["params"][2],
                                json!({
                                    "blockHash": history_hash,
                                    "requireCanonical": true,
                                })
                            );
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": storage_value,
                            })
                        }
                        "head" => json!({"jsonrpc": "2.0", "id": id, "result": "0x3eb"}),
                        "common" => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"hash": common_hash},
                        }),
                        unexpected => {
                            panic!("unexpected archive readiness request: {unexpected}")
                        }
                    }
                }
            }
        })
        .await;
        let router = RpcRouter::new(
            RouterConfig {
                live_history_start: LIVE_START,
                max_response_bytes: 64 * 1024,
                max_batch_len: 8,
                max_inflight: 16,
                backend_timeout: Duration::from_secs(2),
            },
            BackendConfig { name: "live", url: live_url },
            BackendConfig { name: "archive", url: archive_url },
        )
        .unwrap();
        let ready = readiness(
            &router,
            ReadinessConfig {
                expected_chain_id: 40,
                anchor_hash: &anchor_hash,
                history_probe_number: LIVE_START - 1,
                history_probe_hash: &history_hash,
                history_probe_address: &address,
                history_probe_balance: balance,
                history_probe_transaction_hash: &transaction_hash,
                history_storage_probe_address: &storage_address,
                history_storage_probe_slot: "0x2",
                history_storage_probe_value: &storage_value,
                max_head_lag: 4,
            },
        )
        .await
        .unwrap();
        assert_eq!(ready["ready"], true);
        assert_eq!(ready["history_probe_balance"], balance);
        assert_eq!(ready["history_probe_transaction_hash"], transaction_hash);
        assert_eq!(ready["history_storage_probe_address"], storage_address);
        assert_eq!(ready["history_storage_probe_slot"], "0x2");
        assert_eq!(ready["history_storage_probe_value"], storage_value);
        assert_eq!(ready["common_hash"], common_hash);
        live_task.abort();
        archive_task.abort();
    }

    #[test]
    fn receipt_probe_rejects_wrong_block_transaction_or_number() {
        let transaction_hash = format!("0x{}", "11".repeat(32));
        let block_hash = format!("0x{}", "22".repeat(32));
        let response = Some(json!({
            "jsonrpc": "2.0",
            "id": "history-receipt",
            "result": {
                "transactionHash": transaction_hash,
                "blockHash": block_hash,
                "blockNumber": "0x3e6",
            },
        }));
        assert!(response_receipt_identity(
            response,
            "archive",
            &format!("0x{}", "11".repeat(32)),
            &format!("0x{}", "22".repeat(32)),
            LIVE_START - 1,
        )
        .is_err());
    }
}
