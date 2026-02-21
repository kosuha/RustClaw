use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap};
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256, Sha512};
use tokio::time::sleep;
use urlencoding::encode;
use uuid::Uuid;

use crate::env_file::read_env_var;

const UPBIT_DEFAULT_BASE_URL: &str = "https://api.upbit.com";
const UPBIT_POST_CONTENT_TYPE: &str = "application/json; charset=utf-8";
const UPBIT_DEFAULT_MAX_RETRIES: u32 = 3;
const UPBIT_DEFAULT_BACKOFF_MS: u64 = 250;
const UPBIT_MAX_BACKOFF_MS: u64 = 4_000;
const UPBIT_BLOCK_FALLBACK_MS: u64 = 60_000;
const UPBIT_TASK_TYPE: &str = "upbit_proxy_request";
const UPBIT_TASK_ACTION: &str = "http_request";
const UPBIT_IDEMPOTENCY_DIR: &str = "upbit_idempotency";
const UPBIT_RESPONSES_DIR: &str = "responses";

type HmacSha512 = Hmac<Sha512>;

#[derive(Debug, Clone, Deserialize)]
struct UpbitProxyTask {
    #[serde(rename = "type")]
    task_type: String,
    #[serde(rename = "requestId", alias = "request_id")]
    request_id: String,
    action: String,
    request: UpbitHttpRequest,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpbitHttpRequest {
    pathname: String,
    method: Option<String>,
    query: Option<Map<String, Value>>,
    body: Option<Map<String, Value>>,
    #[serde(default)]
    requires_auth: bool,
    rate_limit_group_hint: Option<String>,
    max_retries: Option<u32>,
    intent: Option<UpbitRequestIntent>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpbitRequestIntent {
    confirm_real_order: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpbitHttpMethod {
    Get,
    Post,
    Delete,
}

impl UpbitHttpMethod {
    fn from_input(raw: Option<&str>) -> Result<Self, String> {
        let value = raw.unwrap_or("GET").trim().to_ascii_uppercase();
        match value.as_str() {
            "GET" => Ok(Self::Get),
            "POST" => Ok(Self::Post),
            "DELETE" => Ok(Self::Delete),
            _ => Err(format!("unsupported method: {value}")),
        }
    }

    fn as_reqwest(self) -> Method {
        match self {
            Self::Get => Method::GET,
            Self::Post => Method::POST,
            Self::Delete => Method::DELETE,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Delete => "DELETE",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AllowedEndpoint {
    Markets,
    Ticker,
    Orderbook,
    TradesTicks,
    CandleMinutes,
    CandleDays,
    CandleWeeks,
    CandleMonths,
    CandleYears,
    Accounts,
    OrderChance,
    OrderGet,
    OrderCancel,
    OpenOrders,
    ClosedOrders,
    OrderTest,
    OrderCreate,
}

impl AllowedEndpoint {
    fn classify(method: UpbitHttpMethod, pathname: &str) -> Option<Self> {
        match (method, pathname) {
            (UpbitHttpMethod::Get, "/v1/market/all") => Some(Self::Markets),
            (UpbitHttpMethod::Get, "/v1/ticker") => Some(Self::Ticker),
            (UpbitHttpMethod::Get, "/v1/orderbook") => Some(Self::Orderbook),
            (UpbitHttpMethod::Get, "/v1/trades/ticks") => Some(Self::TradesTicks),
            (UpbitHttpMethod::Get, "/v1/candles/days") => Some(Self::CandleDays),
            (UpbitHttpMethod::Get, "/v1/candles/weeks") => Some(Self::CandleWeeks),
            (UpbitHttpMethod::Get, "/v1/candles/months") => Some(Self::CandleMonths),
            (UpbitHttpMethod::Get, "/v1/candles/years") => Some(Self::CandleYears),
            (UpbitHttpMethod::Get, path) if is_candle_minutes_path(path) => {
                Some(Self::CandleMinutes)
            }
            (UpbitHttpMethod::Get, "/v1/accounts") => Some(Self::Accounts),
            (UpbitHttpMethod::Get, "/v1/orders/chance") => Some(Self::OrderChance),
            (UpbitHttpMethod::Get, "/v1/order") => Some(Self::OrderGet),
            (UpbitHttpMethod::Delete, "/v1/order") => Some(Self::OrderCancel),
            (UpbitHttpMethod::Get, "/v1/orders/open") => Some(Self::OpenOrders),
            (UpbitHttpMethod::Get, "/v1/orders/closed") => Some(Self::ClosedOrders),
            (UpbitHttpMethod::Post, "/v1/orders/test") => Some(Self::OrderTest),
            (UpbitHttpMethod::Post, "/v1/orders") => Some(Self::OrderCreate),
            _ => None,
        }
    }

    fn requires_auth(self) -> bool {
        matches!(
            self,
            Self::Accounts
                | Self::OrderChance
                | Self::OrderGet
                | Self::OrderCancel
                | Self::OpenOrders
                | Self::ClosedOrders
                | Self::OrderTest
                | Self::OrderCreate
        )
    }

    fn rate_limit_group(self) -> &'static str {
        match self {
            Self::Markets => "market",
            Self::Ticker => "ticker",
            Self::Orderbook => "orderbook",
            Self::TradesTicks => "trade",
            Self::CandleMinutes
            | Self::CandleDays
            | Self::CandleWeeks
            | Self::CandleMonths
            | Self::CandleYears => "candle",
            Self::Accounts
            | Self::OrderChance
            | Self::OrderGet
            | Self::OrderCancel
            | Self::OpenOrders
            | Self::ClosedOrders => "default",
            Self::OrderTest => "order-test",
            Self::OrderCreate => "order",
        }
    }
}

#[derive(Debug, Clone)]
struct PreparedRequest {
    endpoint: AllowedEndpoint,
    method: UpbitHttpMethod,
    path: String,
    query_pairs: Vec<(String, String)>,
    body_pairs: Vec<(String, String)>,
    body_json: Option<String>,
    max_retries: u32,
    idempotency_key: Option<String>,
}

#[derive(Debug, Default)]
struct RateLimitState {
    global_blocked_until_ms: i64,
    buckets: HashMap<String, RateLimitBucket>,
}

#[derive(Debug, Default)]
struct RateLimitBucket {
    sec_remaining: Option<i64>,
    updated_at_ms: i64,
    blocked_until_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IdempotencyRecord {
    idempotency_key: String,
    request_hash: String,
    response: Value,
    created_at: String,
}

static RATE_LIMITER: OnceLock<Mutex<RateLimitState>> = OnceLock::new();
static HTTP_CLIENT: OnceLock<Client> = OnceLock::new();

pub fn is_upbit_proxy_request(payload: &Value) -> bool {
    payload
        .get("type")
        .and_then(Value::as_str)
        .map(|value| value == UPBIT_TASK_TYPE)
        .unwrap_or(false)
}

pub async fn process_upbit_proxy_request(
    ipc_base_dir: &Path,
    source_group: &str,
    payload: &Value,
) -> Result<(), String> {
    let task = parse_proxy_task(payload)?;
    let request_id = task.request_id.clone();

    let response = match execute_proxy_task(ipc_base_dir, source_group, &task).await {
        Ok(result) => UpbitProxyResponse::ok(&request_id, result),
        Err(error) => UpbitProxyResponse::err(&request_id, error),
    };

    write_proxy_response(ipc_base_dir, source_group, &response)
}

fn parse_proxy_task(payload: &Value) -> Result<UpbitProxyTask, String> {
    let task = serde_json::from_value::<UpbitProxyTask>(payload.clone())
        .map_err(|err| format!("invalid upbit_proxy_request payload: {err}"))?;
    if task.task_type != UPBIT_TASK_TYPE {
        return Err(format!(
            "unexpected upbit proxy task type: {}",
            task.task_type
        ));
    }
    if task.action != UPBIT_TASK_ACTION {
        return Err(format!("unsupported upbit proxy action: {}", task.action));
    }
    validate_request_id(&task.request_id)?;
    Ok(task)
}

async fn execute_proxy_task(
    ipc_base_dir: &Path,
    source_group: &str,
    task: &UpbitProxyTask,
) -> Result<Value, String> {
    let prepared = prepare_request(&task.request)?;

    if prepared.endpoint == AllowedEndpoint::OrderCreate {
        let Some(idempotency_key) = prepared.idempotency_key.clone() else {
            return Err("real order requires identifier for idempotency.".to_string());
        };
        let request_hash = sha256_hex(format!(
            "{}:{}:{}",
            prepared.method.as_str(),
            prepared.path,
            build_raw_query_string(&prepared.body_pairs)
        ));

        if let Some(record) = load_idempotency_record(ipc_base_dir, source_group, &idempotency_key)?
        {
            if record.request_hash != request_hash {
                return Err("identifier already used with a different order payload.".to_string());
            }
            return Ok(record.response);
        }

        let response = execute_upbit_request(&prepared).await?;
        let record = IdempotencyRecord {
            idempotency_key: idempotency_key.clone(),
            request_hash,
            response: response.clone(),
            created_at: Utc::now().to_rfc3339(),
        };
        save_idempotency_record(ipc_base_dir, source_group, &record)?;
        return Ok(response);
    }

    execute_upbit_request(&prepared).await
}

fn prepare_request(request: &UpbitHttpRequest) -> Result<PreparedRequest, String> {
    let method = UpbitHttpMethod::from_input(request.method.as_deref())?;
    let path = normalize_path(&request.pathname);
    let endpoint = AllowedEndpoint::classify(method, &path).ok_or_else(|| {
        format!(
            "endpoint not allowlisted: method={} path={}",
            method.as_str(),
            path
        )
    })?;
    if endpoint.requires_auth() != request.requires_auth {
        return Err("invalid requires_auth value for allowlisted endpoint.".to_string());
    }

    let max_retries = clamp_max_retries(endpoint, request.max_retries);
    let _group_hint = request
        .rate_limit_group_hint
        .clone()
        .unwrap_or_else(|| endpoint.rate_limit_group().to_string());

    match endpoint {
        AllowedEndpoint::Markets => {
            let query = request.query.as_ref();
            ensure_allowed_keys(query, &["is_details"])?;
            let mut query_pairs = Vec::new();
            if let Some(value) = query.and_then(|map| map.get("is_details")) {
                let flag = parse_bool_like(value)
                    .ok_or_else(|| "is_details must be boolean or bool-like string.".to_string())?;
                query_pairs.push(("is_details".to_string(), flag.to_string()));
            }

            Ok(PreparedRequest {
                endpoint,
                method,
                path,
                query_pairs,
                body_pairs: Vec::new(),
                body_json: None,
                max_retries,
                idempotency_key: None,
            })
        }
        AllowedEndpoint::Ticker | AllowedEndpoint::Orderbook => {
            let query = request
                .query
                .as_ref()
                .ok_or_else(|| "markets query is required.".to_string())?;
            ensure_allowed_keys(Some(query), &["markets"])?;
            let markets_raw = query
                .get("markets")
                .and_then(value_as_string)
                .ok_or_else(|| "markets must be a comma-separated string.".to_string())?;
            let markets = normalize_markets_csv(&markets_raw)?;
            let query_pairs = vec![("markets".to_string(), markets.join(","))];

            Ok(PreparedRequest {
                endpoint,
                method,
                path,
                query_pairs,
                body_pairs: Vec::new(),
                body_json: None,
                max_retries,
                idempotency_key: None,
            })
        }
        AllowedEndpoint::TradesTicks => {
            let query = request
                .query
                .as_ref()
                .ok_or_else(|| "market query is required.".to_string())?;
            ensure_allowed_keys(
                Some(query),
                &["market", "to", "count", "cursor", "days_ago"],
            )?;
            ensure_allowed_keys(request.body.as_ref(), &[])?;

            let market = query
                .get("market")
                .and_then(value_as_string)
                .ok_or_else(|| "market is required.".to_string())
                .and_then(|raw| normalize_market_code(raw.as_str()))?;

            let mut query_pairs = vec![("market".to_string(), market)];
            if let Some(to) = query.get("to").and_then(value_as_string) {
                if to.is_empty() {
                    return Err("to cannot be empty.".to_string());
                }
                query_pairs.push(("to".to_string(), to));
            }
            let count = query
                .get("count")
                .map(parse_u32_like)
                .transpose()?
                .unwrap_or(1);
            if !(1..=500).contains(&count) {
                return Err("count must be within 1..=500.".to_string());
            }
            query_pairs.push(("count".to_string(), count.to_string()));
            if let Some(cursor) = query.get("cursor").and_then(value_as_string) {
                if cursor.is_empty() {
                    return Err("cursor cannot be empty.".to_string());
                }
                query_pairs.push(("cursor".to_string(), cursor));
            }
            if let Some(days_ago) = query.get("days_ago") {
                let value = parse_u32_like(days_ago)?;
                if !(1..=7).contains(&value) {
                    return Err("days_ago must be within 1..=7.".to_string());
                }
                query_pairs.push(("days_ago".to_string(), value.to_string()));
            }

            Ok(PreparedRequest {
                endpoint,
                method,
                path,
                query_pairs,
                body_pairs: Vec::new(),
                body_json: None,
                max_retries,
                idempotency_key: None,
            })
        }
        AllowedEndpoint::CandleMinutes => {
            let query = request
                .query
                .as_ref()
                .ok_or_else(|| "market query is required.".to_string())?;
            ensure_allowed_keys(Some(query), &["market", "to", "count"])?;
            ensure_allowed_keys(request.body.as_ref(), &[])?;

            let unit = parse_candle_minutes_unit(path.as_str())?;
            let path = format!("/v1/candles/minutes/{unit}");
            let query_pairs = build_candle_query_pairs(query, 200)?;

            Ok(PreparedRequest {
                endpoint,
                method,
                path,
                query_pairs,
                body_pairs: Vec::new(),
                body_json: None,
                max_retries,
                idempotency_key: None,
            })
        }
        AllowedEndpoint::CandleDays => {
            let query = request
                .query
                .as_ref()
                .ok_or_else(|| "market query is required.".to_string())?;
            ensure_allowed_keys(
                Some(query),
                &["market", "to", "count", "converting_price_unit"],
            )?;
            ensure_allowed_keys(request.body.as_ref(), &[])?;

            let mut query_pairs = build_candle_query_pairs(query, 200)?;
            if let Some(unit) = query.get("converting_price_unit").and_then(value_as_string) {
                if unit.is_empty() {
                    return Err("converting_price_unit cannot be empty.".to_string());
                }
                query_pairs.push((
                    "converting_price_unit".to_string(),
                    unit.to_ascii_uppercase(),
                ));
            }

            Ok(PreparedRequest {
                endpoint,
                method,
                path,
                query_pairs,
                body_pairs: Vec::new(),
                body_json: None,
                max_retries,
                idempotency_key: None,
            })
        }
        AllowedEndpoint::CandleWeeks
        | AllowedEndpoint::CandleMonths
        | AllowedEndpoint::CandleYears => {
            let query = request
                .query
                .as_ref()
                .ok_or_else(|| "market query is required.".to_string())?;
            ensure_allowed_keys(Some(query), &["market", "to", "count"])?;
            ensure_allowed_keys(request.body.as_ref(), &[])?;
            let query_pairs = build_candle_query_pairs(query, 200)?;

            Ok(PreparedRequest {
                endpoint,
                method,
                path,
                query_pairs,
                body_pairs: Vec::new(),
                body_json: None,
                max_retries,
                idempotency_key: None,
            })
        }
        AllowedEndpoint::Accounts => {
            ensure_allowed_keys(request.query.as_ref(), &[])?;
            ensure_allowed_keys(request.body.as_ref(), &[])?;
            Ok(PreparedRequest {
                endpoint,
                method,
                path,
                query_pairs: Vec::new(),
                body_pairs: Vec::new(),
                body_json: None,
                max_retries,
                idempotency_key: None,
            })
        }
        AllowedEndpoint::OrderChance => {
            let query = request
                .query
                .as_ref()
                .ok_or_else(|| "market query is required.".to_string())?;
            ensure_allowed_keys(Some(query), &["market"])?;
            ensure_allowed_keys(request.body.as_ref(), &[])?;

            let market = query
                .get("market")
                .and_then(value_as_string)
                .ok_or_else(|| "market is required.".to_string())
                .and_then(|raw| normalize_market_code(raw.as_str()))?;

            Ok(PreparedRequest {
                endpoint,
                method,
                path,
                query_pairs: vec![("market".to_string(), market)],
                body_pairs: Vec::new(),
                body_json: None,
                max_retries,
                idempotency_key: None,
            })
        }
        AllowedEndpoint::OrderGet | AllowedEndpoint::OrderCancel => {
            let query = request
                .query
                .as_ref()
                .ok_or_else(|| "uuid or identifier query is required.".to_string())?;
            ensure_allowed_keys(Some(query), &["uuid", "identifier"])?;
            ensure_allowed_keys(request.body.as_ref(), &[])?;
            let query_pairs = build_order_lookup_query_pairs(Some(query))?;

            Ok(PreparedRequest {
                endpoint,
                method,
                path,
                query_pairs,
                body_pairs: Vec::new(),
                body_json: None,
                max_retries,
                idempotency_key: None,
            })
        }
        AllowedEndpoint::OpenOrders => {
            let query = request.query.as_ref();
            ensure_allowed_keys(
                query,
                &["market", "state", "states[]", "page", "limit", "order_by"],
            )?;
            ensure_allowed_keys(request.body.as_ref(), &[])?;

            let mut query_pairs = Vec::new();
            if let Some(market_value) = query.and_then(|map| map.get("market")) {
                let market = value_as_string(market_value)
                    .ok_or_else(|| "market must be a string.".to_string())?;
                let normalized = normalize_market_code(&market)?;
                query_pairs.push(("market".to_string(), normalized));
            }

            let state_pairs =
                build_state_filter_query_pairs(query, &["wait", "watch"], "state", "states[]")?;
            query_pairs.extend(state_pairs);

            let page = query
                .and_then(|map| map.get("page"))
                .map(parse_u32_like)
                .transpose()?
                .unwrap_or(1);
            if page == 0 {
                return Err("page must be greater than 0.".to_string());
            }
            query_pairs.push(("page".to_string(), page.to_string()));

            let limit = query
                .and_then(|map| map.get("limit"))
                .map(parse_u32_like)
                .transpose()?
                .unwrap_or(100);
            if !(1..=100).contains(&limit) {
                return Err("limit must be within 1..=100.".to_string());
            }
            query_pairs.push(("limit".to_string(), limit.to_string()));

            let order_by = query
                .and_then(|map| map.get("order_by"))
                .and_then(value_as_string)
                .unwrap_or_else(|| "desc".to_string())
                .to_ascii_lowercase();
            if order_by != "asc" && order_by != "desc" {
                return Err("order_by must be asc or desc.".to_string());
            }
            query_pairs.push(("order_by".to_string(), order_by));

            Ok(PreparedRequest {
                endpoint,
                method,
                path,
                query_pairs,
                body_pairs: Vec::new(),
                body_json: None,
                max_retries,
                idempotency_key: None,
            })
        }
        AllowedEndpoint::ClosedOrders => {
            let query = request.query.as_ref();
            ensure_allowed_keys(
                query,
                &[
                    "market",
                    "state",
                    "states[]",
                    "start_time",
                    "end_time",
                    "limit",
                    "order_by",
                ],
            )?;
            ensure_allowed_keys(request.body.as_ref(), &[])?;

            let mut query_pairs = Vec::new();
            if let Some(market_value) = query.and_then(|map| map.get("market")) {
                let market = value_as_string(market_value)
                    .ok_or_else(|| "market must be a string.".to_string())?;
                let normalized = normalize_market_code(&market)?;
                query_pairs.push(("market".to_string(), normalized));
            }

            let state_pairs =
                build_state_filter_query_pairs(query, &["done", "cancel"], "state", "states[]")?;
            query_pairs.extend(state_pairs);

            if let Some(start_time) = query
                .and_then(|map| map.get("start_time"))
                .and_then(value_as_string)
            {
                if start_time.is_empty() {
                    return Err("start_time cannot be empty.".to_string());
                }
                query_pairs.push(("start_time".to_string(), start_time));
            }
            if let Some(end_time) = query
                .and_then(|map| map.get("end_time"))
                .and_then(value_as_string)
            {
                if end_time.is_empty() {
                    return Err("end_time cannot be empty.".to_string());
                }
                query_pairs.push(("end_time".to_string(), end_time));
            }

            let limit = query
                .and_then(|map| map.get("limit"))
                .map(parse_u32_like)
                .transpose()?
                .unwrap_or(100);
            if !(1..=1_000).contains(&limit) {
                return Err("limit must be within 1..=1000.".to_string());
            }
            query_pairs.push(("limit".to_string(), limit.to_string()));

            let order_by = query
                .and_then(|map| map.get("order_by"))
                .and_then(value_as_string)
                .unwrap_or_else(|| "desc".to_string())
                .to_ascii_lowercase();
            if order_by != "asc" && order_by != "desc" {
                return Err("order_by must be asc or desc.".to_string());
            }
            query_pairs.push(("order_by".to_string(), order_by));

            Ok(PreparedRequest {
                endpoint,
                method,
                path,
                query_pairs,
                body_pairs: Vec::new(),
                body_json: None,
                max_retries,
                idempotency_key: None,
            })
        }
        AllowedEndpoint::OrderTest | AllowedEndpoint::OrderCreate => {
            ensure_allowed_keys(request.query.as_ref(), &[])?;
            let body_map = request
                .body
                .as_ref()
                .ok_or_else(|| "order request body is required.".to_string())?;
            ensure_allowed_keys(
                Some(body_map),
                &[
                    "market",
                    "side",
                    "ord_type",
                    "volume",
                    "price",
                    "time_in_force",
                    "smp_type",
                    "identifier",
                ],
            )?;

            let market = normalize_market_code(
                &body_map
                    .get("market")
                    .and_then(value_as_string)
                    .ok_or_else(|| "market is required.".to_string())?,
            )?;
            let side = body_map
                .get("side")
                .and_then(value_as_string)
                .ok_or_else(|| "side is required.".to_string())?
                .to_ascii_lowercase();
            let ord_type = body_map
                .get("ord_type")
                .and_then(value_as_string)
                .ok_or_else(|| "ord_type is required.".to_string())?
                .to_ascii_lowercase();
            let volume = body_map.get("volume").and_then(value_as_string);
            let price = body_map.get("price").and_then(value_as_string);
            let time_in_force = body_map.get("time_in_force").and_then(value_as_string);
            let smp_type = body_map.get("smp_type").and_then(value_as_string);
            let identifier = body_map.get("identifier").and_then(value_as_string);

            validate_order_payload(
                &side,
                &ord_type,
                volume.as_deref(),
                price.as_deref(),
                time_in_force.as_deref(),
                smp_type.as_deref(),
            )?;

            if endpoint == AllowedEndpoint::OrderCreate {
                let confirmed = request
                    .intent
                    .as_ref()
                    .and_then(|intent| intent.confirm_real_order)
                    .unwrap_or(false);
                if !confirmed {
                    return Err(
                        "real order is blocked: confirm_real_order must be true.".to_string()
                    );
                }
            }

            let mut body_pairs = Vec::new();
            body_pairs.push(("market".to_string(), market.clone()));
            body_pairs.push(("side".to_string(), side.clone()));
            body_pairs.push(("ord_type".to_string(), ord_type.clone()));
            if let Some(value) = volume.clone() {
                body_pairs.push(("volume".to_string(), value));
            }
            if let Some(value) = price.clone() {
                body_pairs.push(("price".to_string(), value));
            }
            if let Some(value) = time_in_force.clone() {
                body_pairs.push(("time_in_force".to_string(), value.to_ascii_lowercase()));
            }
            if let Some(value) = smp_type.clone() {
                body_pairs.push(("smp_type".to_string(), value.to_ascii_lowercase()));
            }

            let mut idempotency_key = None;
            if endpoint == AllowedEndpoint::OrderCreate {
                let key = identifier
                    .clone()
                    .ok_or_else(|| "real order requires identifier.".to_string())?;
                idempotency_key = Some(key.clone());
                body_pairs.push(("identifier".to_string(), key));
            } else if let Some(value) = identifier.clone() {
                body_pairs.push(("identifier".to_string(), value));
            }

            let body_json = pairs_to_json_object(&body_pairs)?;

            Ok(PreparedRequest {
                endpoint,
                method,
                path,
                query_pairs: Vec::new(),
                body_pairs,
                body_json: Some(body_json),
                max_retries,
                idempotency_key,
            })
        }
    }
}

async fn execute_upbit_request(request: &PreparedRequest) -> Result<Value, String> {
    let base_url = normalize_base_url(read_env_var("UPBIT_BASE_URL").as_deref());
    let query_encoded = build_encoded_query_string(&request.query_pairs);
    let query_raw = build_raw_query_string(&request.query_pairs);
    let body_raw = build_raw_query_string(&request.body_pairs);

    let max_retries = request.max_retries;
    let mut backoff_ms = UPBIT_DEFAULT_BACKOFF_MS;

    for attempt in 0..=max_retries {
        throttle_for_group(request.endpoint.rate_limit_group()).await;

        let auth_header = if request.endpoint.requires_auth() {
            let credentials = resolve_upbit_credentials()?;
            let query_hash_input =
                build_query_hash_input(request.method, query_raw.as_str(), body_raw.as_str());
            Some(format!(
                "Bearer {}",
                create_jwt(&credentials, query_hash_input.as_str())?
            ))
        } else {
            None
        };

        let url = format!(
            "{}{}{}",
            base_url,
            request.path,
            if query_encoded.is_empty() {
                String::new()
            } else {
                format!("?{query_encoded}")
            }
        );

        let mut builder = http_client()
            .request(request.method.as_reqwest(), url)
            .header(ACCEPT, "application/json");
        if let Some(value) = auth_header {
            builder = builder.header(AUTHORIZATION, value);
        }
        if request.method == UpbitHttpMethod::Post {
            builder = builder.header(CONTENT_TYPE, UPBIT_POST_CONTENT_TYPE);
            if let Some(body_json) = &request.body_json {
                builder = builder.body(body_json.clone());
            }
        }

        let response = match builder.send().await {
            Ok(value) => value,
            Err(err) => {
                if attempt >= max_retries {
                    return Err(format!("upbit network error: {err}"));
                }
                sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms.saturating_mul(2)).min(UPBIT_MAX_BACKOFF_MS);
                continue;
            }
        };

        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .text()
            .await
            .map_err(|err| format!("failed to read upbit response body: {err}"))?;
        let payload = parse_response_payload(&body);
        let detail = error_detail_from_payload(&payload);
        let observed_group = update_remaining_req_from_headers(&headers)
            .unwrap_or_else(|| request.endpoint.rate_limit_group().to_string());

        if status == StatusCode::TOO_MANY_REQUESTS {
            if attempt >= max_retries {
                return Err(format!("upbit rate limited (429): {detail}"));
            }
            let sleep_ms = parse_retry_after_ms(&headers).unwrap_or(backoff_ms);
            sleep(Duration::from_millis(sleep_ms.max(150))).await;
            backoff_ms = (backoff_ms.saturating_mul(2)).min(UPBIT_MAX_BACKOFF_MS);
            continue;
        }

        if status == StatusCode::from_u16(418).expect("valid status code") {
            let block_ms = parse_retry_after_ms(&headers)
                .or_else(|| parse_block_window_from_detail(&detail))
                .unwrap_or(UPBIT_BLOCK_FALLBACK_MS);
            remember_temporary_block(&observed_group, block_ms);
            let seconds = (block_ms as f64 / 1000.0).ceil() as i64;
            return Err(format!(
                "upbit temporary block (418), retry after ~{seconds}s"
            ));
        }

        if !status.is_success() {
            return Err(format!(
                "upbit request failed ({}): {detail}",
                status.as_u16()
            ));
        }

        return Ok(payload);
    }

    Err("upbit request failed after retries.".to_string())
}

fn clamp_max_retries(endpoint: AllowedEndpoint, requested: Option<u32>) -> u32 {
    let requested = requested.unwrap_or(UPBIT_DEFAULT_MAX_RETRIES);
    if endpoint == AllowedEndpoint::OrderCreate {
        return 0;
    }
    requested.min(UPBIT_DEFAULT_MAX_RETRIES)
}

fn parse_response_payload(body: &str) -> Value {
    if body.trim().is_empty() {
        return Value::Null;
    }
    serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.to_string()))
}

fn error_detail_from_payload(payload: &Value) -> String {
    if let Value::String(text) = payload {
        return text.clone();
    }
    if let Some(error_message) = payload
        .get("error")
        .and_then(Value::as_object)
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
    {
        let error_name = payload
            .get("error")
            .and_then(Value::as_object)
            .and_then(|error| error.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("error");
        return format!("{error_name}: {error_message}");
    }
    payload.to_string()
}

fn resolve_upbit_credentials() -> Result<(String, String), String> {
    let access_key = read_env_var("UPBIT_ACCESS_KEY").ok_or_else(|| {
        "missing UPBIT_ACCESS_KEY on host environment for authenticated upbit request.".to_string()
    })?;
    let secret_key = read_env_var("UPBIT_SECRET_KEY").ok_or_else(|| {
        "missing UPBIT_SECRET_KEY on host environment for authenticated upbit request.".to_string()
    })?;
    Ok((access_key, secret_key))
}

fn normalize_base_url(raw: Option<&str>) -> String {
    let value = raw.unwrap_or(UPBIT_DEFAULT_BASE_URL).trim();
    value.trim_end_matches('/').to_string()
}

fn build_query_hash_input(method: UpbitHttpMethod, query_raw: &str, body_raw: &str) -> String {
    if method == UpbitHttpMethod::Get || method == UpbitHttpMethod::Delete {
        return query_raw.to_string();
    }
    if !query_raw.is_empty() && !body_raw.is_empty() {
        return format!("{query_raw}&{body_raw}");
    }
    if !body_raw.is_empty() {
        return body_raw.to_string();
    }
    query_raw.to_string()
}

fn create_jwt(credentials: &(String, String), query_hash_input: &str) -> Result<String, String> {
    let (access_key, secret_key) = credentials;
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"HS512","typ":"JWT"}"#.as_bytes());

    let mut payload = Map::new();
    payload.insert("access_key".to_string(), Value::String(access_key.clone()));
    payload.insert(
        "nonce".to_string(),
        Value::String(Uuid::new_v4().to_string()),
    );
    if !query_hash_input.is_empty() {
        let query_hash = Sha512::digest(query_hash_input.as_bytes());
        payload.insert(
            "query_hash".to_string(),
            Value::String(hex_string(query_hash.as_slice())),
        );
        payload.insert(
            "query_hash_alg".to_string(),
            Value::String("SHA512".to_string()),
        );
    }
    let payload_bytes = serde_json::to_vec(&payload)
        .map_err(|err| format!("failed to encode upbit jwt payload: {err}"))?;
    let payload = URL_SAFE_NO_PAD.encode(payload_bytes);

    let signing_input = format!("{header}.{payload}");
    let mut mac = HmacSha512::new_from_slice(secret_key.as_bytes())
        .map_err(|err| format!("failed to initialize upbit hmac: {err}"))?;
    mac.update(signing_input.as_bytes());
    let signature = mac.finalize().into_bytes();
    let signature = URL_SAFE_NO_PAD.encode(signature);

    Ok(format!("{signing_input}.{signature}"))
}

fn build_encoded_query_string(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                encode_component(key.as_str()),
                encode_component(value.as_str())
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn build_raw_query_string(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn encode_component(value: &str) -> String {
    encode(value)
        .into_owned()
        .replace("%5B", "[")
        .replace("%5D", "]")
}

fn hex_string(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn sha256_hex(input: String) -> String {
    let digest = Sha256::digest(input.as_bytes());
    hex_string(digest.as_slice())
}

fn ensure_allowed_keys(value: Option<&Map<String, Value>>, allowed: &[&str]) -> Result<(), String> {
    let Some(map) = value else {
        return Ok(());
    };
    for key in map.keys() {
        if !allowed.iter().any(|allowed_key| *allowed_key == key) {
            return Err(format!("unsupported field: {key}"));
        }
    }
    Ok(())
}

fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().to_string()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn parse_bool_like(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn parse_u32_like(value: &Value) -> Result<u32, String> {
    match value {
        Value::Number(number) => number
            .as_u64()
            .and_then(|raw| raw.try_into().ok())
            .ok_or_else(|| "number must be an unsigned integer".to_string()),
        Value::String(text) => text
            .trim()
            .parse::<u32>()
            .map_err(|_| format!("invalid integer: {text}")),
        _ => Err("value must be integer-like".to_string()),
    }
}

fn normalize_markets_csv(markets_csv: &str) -> Result<Vec<String>, String> {
    let markets = markets_csv
        .split(',')
        .map(|value| value.trim().to_ascii_uppercase())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if markets.is_empty() {
        return Err("at least one market code is required.".to_string());
    }
    if markets.len() > 20 {
        return Err("at most 20 markets can be requested at once.".to_string());
    }
    for market in &markets {
        normalize_market_code(market)?;
    }
    Ok(markets)
}

fn normalize_market_code(raw: &str) -> Result<String, String> {
    let value = raw.trim().to_ascii_uppercase();
    if value.is_empty() || !value.contains('-') {
        return Err(format!("invalid market code: {raw}"));
    }
    Ok(value)
}

fn build_candle_query_pairs(
    query: &Map<String, Value>,
    max_count: u32,
) -> Result<Vec<(String, String)>, String> {
    let market = query
        .get("market")
        .and_then(value_as_string)
        .ok_or_else(|| "market is required.".to_string())
        .and_then(|raw| normalize_market_code(raw.as_str()))?;

    let mut query_pairs = vec![("market".to_string(), market)];
    if let Some(to) = query.get("to").and_then(value_as_string) {
        if to.is_empty() {
            return Err("to cannot be empty.".to_string());
        }
        query_pairs.push(("to".to_string(), to));
    }

    let count = query
        .get("count")
        .map(parse_u32_like)
        .transpose()?
        .unwrap_or(1);
    if !(1..=max_count).contains(&count) {
        return Err(format!("count must be within 1..={max_count}."));
    }
    query_pairs.push(("count".to_string(), count.to_string()));
    Ok(query_pairs)
}

fn is_candle_minutes_path(path: &str) -> bool {
    parse_candle_minutes_unit(path).is_ok()
}

fn parse_candle_minutes_unit(path: &str) -> Result<u32, String> {
    const PREFIX: &str = "/v1/candles/minutes/";
    let Some(unit_raw) = path.strip_prefix(PREFIX) else {
        return Err("invalid minute candles path.".to_string());
    };
    if unit_raw.is_empty() || unit_raw.contains('/') {
        return Err("invalid minute candles unit path.".to_string());
    }
    let unit = unit_raw
        .parse::<u32>()
        .map_err(|_| format!("invalid minute candle unit: {unit_raw}"))?;
    if !matches!(unit, 1 | 3 | 5 | 10 | 15 | 30 | 60 | 240) {
        return Err(format!("unsupported minute candle unit: {unit}"));
    }
    Ok(unit)
}

fn build_order_lookup_query_pairs(
    query: Option<&Map<String, Value>>,
) -> Result<Vec<(String, String)>, String> {
    let Some(map) = query else {
        return Err("uuid or identifier query is required.".to_string());
    };
    let uuid = map
        .get("uuid")
        .and_then(value_as_string)
        .filter(|value| !value.is_empty());
    let identifier = map
        .get("identifier")
        .and_then(value_as_string)
        .filter(|value| !value.is_empty());

    match (uuid, identifier) {
        (Some(_), Some(_)) => Err("provide only one of uuid or identifier.".to_string()),
        (Some(uuid), None) => Ok(vec![("uuid".to_string(), uuid)]),
        (None, Some(identifier)) => Ok(vec![("identifier".to_string(), identifier)]),
        (None, None) => Err("either uuid or identifier is required.".to_string()),
    }
}

fn build_state_filter_query_pairs(
    query: Option<&Map<String, Value>>,
    allowed_states: &[&str],
    single_key: &str,
    multi_key: &str,
) -> Result<Vec<(String, String)>, String> {
    let Some(map) = query else {
        return Ok(Vec::new());
    };
    let single = map.get(single_key);
    let multiple = map.get(multi_key);
    if single.is_some() && multiple.is_some() {
        return Err(format!("cannot use {single_key} and {multi_key} together."));
    }

    if let Some(value) = single {
        let states = parse_state_values(value, allowed_states, single_key)?;
        return Ok(vec![(single_key.to_string(), states[0].clone())]);
    }
    if let Some(value) = multiple {
        let states = parse_state_values(value, allowed_states, multi_key)?;
        return Ok(states
            .into_iter()
            .map(|state| (multi_key.to_string(), state))
            .collect());
    }
    Ok(Vec::new())
}

fn parse_state_values(
    value: &Value,
    allowed_states: &[&str],
    field_name: &str,
) -> Result<Vec<String>, String> {
    let values = match value {
        Value::Array(items) => items
            .iter()
            .filter_map(value_as_string)
            .collect::<Vec<String>>(),
        _ => value_as_string(value)
            .map(|single| vec![single])
            .unwrap_or_default(),
    };
    if values.is_empty() {
        return Err(format!("{field_name} cannot be empty."));
    }

    let mut normalized = Vec::new();
    for state in values {
        let state = state.to_ascii_lowercase();
        if !allowed_states
            .iter()
            .any(|allowed| *allowed == state.as_str())
        {
            return Err(format!(
                "{field_name} supports only {}.",
                allowed_states.join(", ")
            ));
        }
        if !normalized.iter().any(|existing| existing == &state) {
            normalized.push(state);
        }
    }
    Ok(normalized)
}

fn validate_order_payload(
    side: &str,
    ord_type: &str,
    volume: Option<&str>,
    price: Option<&str>,
    time_in_force: Option<&str>,
    smp_type: Option<&str>,
) -> Result<(), String> {
    let side = side.to_ascii_lowercase();
    let ord_type = ord_type.to_ascii_lowercase();
    let tif = time_in_force.map(|value| value.to_ascii_lowercase());
    let smp = smp_type.map(|value| value.to_ascii_lowercase());
    let volume_present = volume.map(is_positive_number).unwrap_or(false);
    let price_present = price.map(is_positive_number).unwrap_or(false);

    if side != "bid" && side != "ask" {
        return Err("side must be bid or ask.".to_string());
    }
    if !matches!(ord_type.as_str(), "limit" | "price" | "market" | "best") {
        return Err("ord_type must be one of limit, price, market, best.".to_string());
    }

    if let Some(tif) = tif.as_deref() {
        match tif {
            "ioc" | "fok" | "post_only" => {}
            _ => return Err("time_in_force must be ioc, fok, or post_only.".to_string()),
        }
        if ord_type == "price" || ord_type == "market" {
            return Err("time_in_force is only supported for limit/best orders.".to_string());
        }
    }
    if let Some(smp) = smp.as_deref() {
        if !matches!(smp, "cancel_maker" | "cancel_taker" | "reduce") {
            return Err("smp_type must be cancel_maker, cancel_taker, or reduce.".to_string());
        }
    }
    if tif.as_deref() == Some("post_only") && smp.is_some() {
        return Err("post_only cannot be used together with smp_type.".to_string());
    }

    match (side.as_str(), ord_type.as_str()) {
        ("bid", "limit") | ("ask", "limit") => {
            if !volume_present {
                return Err("limit order requires positive volume.".to_string());
            }
            if !price_present {
                return Err("limit order requires positive price.".to_string());
            }
            if let Some(tif) = tif.as_deref() {
                if !matches!(tif, "ioc" | "fok" | "post_only") {
                    return Err("invalid limit order time_in_force.".to_string());
                }
            }
        }
        ("bid", "price") => {
            if !price_present {
                return Err("market buy (ord_type=price) requires positive price.".to_string());
            }
            if volume.is_some() {
                return Err("market buy (ord_type=price) must not include volume.".to_string());
            }
        }
        ("ask", "market") => {
            if !volume_present {
                return Err("market sell (ord_type=market) requires positive volume.".to_string());
            }
            if price.is_some() {
                return Err("market sell (ord_type=market) must not include price.".to_string());
            }
        }
        ("bid", "best") => {
            if !price_present {
                return Err("best bid order requires positive price.".to_string());
            }
            if volume.is_some() {
                return Err("best bid order must not include volume.".to_string());
            }
            if !matches!(tif.as_deref(), Some("ioc" | "fok")) {
                return Err("best order requires time_in_force ioc or fok.".to_string());
            }
        }
        ("ask", "best") => {
            if !volume_present {
                return Err("best ask order requires positive volume.".to_string());
            }
            if price.is_some() {
                return Err("best ask order must not include price.".to_string());
            }
            if !matches!(tif.as_deref(), Some("ioc" | "fok")) {
                return Err("best order requires time_in_force ioc or fok.".to_string());
            }
        }
        _ => {
            return Err(format!(
                "unsupported side/ord_type combination: side={side}, ord_type={ord_type}"
            ));
        }
    }

    Ok(())
}

fn is_positive_number(raw: &str) -> bool {
    raw.trim()
        .parse::<f64>()
        .map(|value| value.is_finite() && value > 0.0)
        .unwrap_or(false)
}

fn pairs_to_json_object(pairs: &[(String, String)]) -> Result<String, String> {
    let mut body = String::from("{");
    for (idx, (key, value)) in pairs.iter().enumerate() {
        if idx > 0 {
            body.push(',');
        }
        let encoded_key = serde_json::to_string(key)
            .map_err(|err| format!("failed to serialize order field key: {err}"))?;
        let encoded_value = serde_json::to_string(value)
            .map_err(|err| format!("failed to serialize order field value: {err}"))?;
        body.push_str(encoded_key.as_str());
        body.push(':');
        body.push_str(encoded_value.as_str());
    }
    body.push('}');
    Ok(body)
}

fn rate_state() -> &'static Mutex<RateLimitState> {
    RATE_LIMITER.get_or_init(|| Mutex::new(RateLimitState::default()))
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

async fn throttle_for_group(group: &str) {
    let wait_ms = {
        let now = now_ms();
        let state = rate_state().lock().expect("rate state lock");
        let mut wait_ms = 0_i64;
        if state.global_blocked_until_ms > now {
            wait_ms = wait_ms.max(state.global_blocked_until_ms - now);
        }
        if let Some(bucket) = state.buckets.get(group) {
            if bucket.blocked_until_ms > now {
                wait_ms = wait_ms.max(bucket.blocked_until_ms - now);
            }
            if bucket.sec_remaining.unwrap_or(1) <= 0 {
                let elapsed = now.saturating_sub(bucket.updated_at_ms);
                if elapsed < 1000 {
                    wait_ms = wait_ms.max(1050 - elapsed);
                }
            }
        }
        wait_ms
    };

    if wait_ms > 0 {
        sleep(Duration::from_millis(wait_ms as u64)).await;
    }
}

fn update_remaining_req_from_headers(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("Remaining-Req")?.to_str().ok()?;
    let mut group = "default".to_string();
    let mut sec_remaining = None::<i64>;
    for section in raw.split(';') {
        let mut iter = section.splitn(2, '=');
        let key = iter.next()?.trim().to_ascii_lowercase();
        let value = iter.next()?.trim();
        if key == "group" {
            group = value.to_string();
            continue;
        }
        if key == "sec" {
            sec_remaining = value.parse::<i64>().ok();
        }
    }

    let now = now_ms();
    let mut state = rate_state().lock().expect("rate state lock");
    let bucket = state.buckets.entry(group.clone()).or_default();
    bucket.sec_remaining = sec_remaining;
    bucket.updated_at_ms = now;
    Some(group)
}

fn remember_temporary_block(group: &str, wait_ms: u64) {
    let until_ms = now_ms().saturating_add(wait_ms as i64);
    let mut state = rate_state().lock().expect("rate state lock");
    state.global_blocked_until_ms = state.global_blocked_until_ms.max(until_ms);
    let bucket = state.buckets.entry(group.to_string()).or_default();
    bucket.blocked_until_ms = bucket.blocked_until_ms.max(until_ms);
}

fn parse_retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    let header = headers.get("Retry-After")?.to_str().ok()?.trim();
    if let Ok(seconds) = header.parse::<u64>() {
        return Some(seconds.saturating_mul(1000));
    }

    DateTime::parse_from_rfc2822(header).ok().and_then(|date| {
        let now = Utc::now();
        let duration = date.with_timezone(&Utc).signed_duration_since(now);
        if duration.num_milliseconds() <= 0 {
            Some(0)
        } else {
            Some(duration.num_milliseconds() as u64)
        }
    })
}

fn parse_block_window_from_detail(detail: &str) -> Option<u64> {
    let lowered = detail.to_ascii_lowercase();
    for token in lowered.split(|ch: char| !ch.is_ascii_alphanumeric()) {
        if let Ok(value) = token.parse::<u64>() {
            if lowered.contains("ms") {
                return Some(value);
            }
            if lowered.contains("sec") || lowered.contains("초") {
                return Some(value.saturating_mul(1_000));
            }
            if lowered.contains("min") || lowered.contains("분") {
                return Some(value.saturating_mul(60_000));
            }
        }
    }
    None
}

fn http_client() -> &'static Client {
    HTTP_CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("build reqwest client")
    })
}

fn load_idempotency_record(
    ipc_base_dir: &Path,
    source_group: &str,
    key: &str,
) -> Result<Option<IdempotencyRecord>, String> {
    let path = idempotency_record_path(ipc_base_dir, source_group, key);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).map_err(|err| {
        format!(
            "failed to read idempotency record {}: {err}",
            path.display()
        )
    })?;
    let parsed = serde_json::from_str(&raw).map_err(|err| {
        format!(
            "failed to parse idempotency record {}: {err}",
            path.display()
        )
    })?;
    Ok(Some(parsed))
}

fn save_idempotency_record(
    ipc_base_dir: &Path,
    source_group: &str,
    record: &IdempotencyRecord,
) -> Result<(), String> {
    let dir = idempotency_dir(ipc_base_dir, source_group);
    fs::create_dir_all(&dir)
        .map_err(|err| format!("failed to create idempotency dir {}: {err}", dir.display()))?;
    let path = idempotency_record_path(ipc_base_dir, source_group, &record.idempotency_key);
    let temp = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(record)
        .map_err(|err| format!("failed to serialize idempotency record: {err}"))?;
    fs::write(&temp, content).map_err(|err| {
        format!(
            "failed to write idempotency temp file {}: {err}",
            temp.display()
        )
    })?;
    fs::rename(&temp, &path).map_err(|err| {
        format!(
            "failed to finalize idempotency record {}: {err}",
            path.display()
        )
    })?;
    Ok(())
}

fn idempotency_dir(ipc_base_dir: &Path, source_group: &str) -> PathBuf {
    ipc_base_dir.join(source_group).join(UPBIT_IDEMPOTENCY_DIR)
}

fn idempotency_record_path(ipc_base_dir: &Path, source_group: &str, key: &str) -> PathBuf {
    let key_hash = sha256_hex(key.to_string());
    idempotency_dir(ipc_base_dir, source_group).join(format!("{key_hash}.json"))
}

#[derive(Debug, Clone, Serialize)]
struct UpbitProxyResponse {
    #[serde(rename = "requestId")]
    request_id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl UpbitProxyResponse {
    fn ok(request_id: &str, result: Value) -> Self {
        Self {
            request_id: request_id.to_string(),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    fn err(request_id: &str, error: String) -> Self {
        Self {
            request_id: request_id.to_string(),
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

fn write_proxy_response(
    ipc_base_dir: &Path,
    source_group: &str,
    response: &UpbitProxyResponse,
) -> Result<(), String> {
    validate_request_id(response.request_id.as_str())?;
    let dir = ipc_base_dir.join(source_group).join(UPBIT_RESPONSES_DIR);
    fs::create_dir_all(&dir).map_err(|err| {
        format!(
            "failed to create upbit response dir {}: {err}",
            dir.display()
        )
    })?;

    let target = dir.join(format!("{}.json", response.request_id));
    let temp = target.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(response).map_err(|err| {
        format!(
            "failed to serialize upbit proxy response {}: {err}",
            response.request_id
        )
    })?;
    fs::write(&temp, content).map_err(|err| {
        format!(
            "failed to write upbit response temp file {}: {err}",
            temp.display()
        )
    })?;
    fs::rename(&temp, &target).map_err(|err| {
        format!(
            "failed to finalize upbit proxy response {}: {err}",
            target.display()
        )
    })?;
    Ok(())
}

fn validate_request_id(request_id: &str) -> Result<(), String> {
    if request_id.is_empty() {
        return Err("request_id is required.".to_string());
    }
    if !request_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err("request_id contains unsupported characters.".to_string());
    }
    Ok(())
}

fn normalize_path(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn market_code_validation_requires_dash() {
        assert!(normalize_market_code("KRW-BTC").is_ok());
        assert!(normalize_market_code("KRWBTC").is_err());
    }

    #[test]
    fn order_validation_enforces_best_tif_rule() {
        let err = validate_order_payload("bid", "best", None, Some("1000"), None, None);
        assert!(err.is_err());
    }

    #[test]
    fn state_parser_enforces_allowed_values() {
        let ok = parse_state_values(&json!(["wait", "watch"]), &["wait", "watch"], "states[]")
            .expect("parse states");
        assert_eq!(ok, vec!["wait".to_string(), "watch".to_string()]);
        assert!(parse_state_values(&json!(["done"]), &["wait", "watch"], "states[]").is_err());
    }

    #[test]
    fn state_filter_rejects_mixed_single_and_multi_keys() {
        let query = json!({
            "state": "wait",
            "states[]": ["wait", "watch"]
        });
        let map = query.as_object().expect("query map");
        let err =
            build_state_filter_query_pairs(Some(map), &["wait", "watch"], "state", "states[]")
                .expect_err("mixed key should fail");
        assert!(err.contains("cannot use state and states[] together"));
    }

    #[test]
    fn order_lookup_requires_single_selector() {
        let empty = json!({});
        let empty_map = empty.as_object().expect("empty map");
        assert!(build_order_lookup_query_pairs(Some(empty_map)).is_err());

        let both = json!({"uuid": "a", "identifier": "b"});
        let both_map = both.as_object().expect("both map");
        assert!(build_order_lookup_query_pairs(Some(both_map)).is_err());

        let only_uuid = json!({"uuid": "abc"});
        let only_uuid_map = only_uuid.as_object().expect("uuid map");
        let pairs = build_order_lookup_query_pairs(Some(only_uuid_map)).expect("uuid parse");
        assert_eq!(pairs, vec![("uuid".to_string(), "abc".to_string())]);
    }

    #[test]
    fn candle_minutes_unit_parser_validates_supported_units() {
        let ok = parse_candle_minutes_unit("/v1/candles/minutes/15").expect("unit parse");
        assert_eq!(ok, 15);
        assert!(parse_candle_minutes_unit("/v1/candles/minutes/2").is_err());
        assert!(parse_candle_minutes_unit("/v1/candles/minutes/15/extra").is_err());
    }

    #[test]
    fn candle_query_builder_requires_market_and_count_range() {
        let query = json!({
            "market": "KRW-BTC",
            "count": 200
        });
        let map = query.as_object().expect("query map");
        let pairs = build_candle_query_pairs(map, 200).expect("candle query");
        assert_eq!(
            pairs,
            vec![
                ("market".to_string(), "KRW-BTC".to_string()),
                ("count".to_string(), "200".to_string())
            ]
        );

        let bad = json!({"market": "KRW-BTC", "count": 201});
        let bad_map = bad.as_object().expect("bad map");
        assert!(build_candle_query_pairs(bad_map, 200).is_err());
    }

    #[test]
    fn pairs_to_json_object_keeps_pair_order() {
        let pairs = vec![
            ("market".to_string(), "KRW-BTC".to_string()),
            ("side".to_string(), "bid".to_string()),
            ("ord_type".to_string(), "price".to_string()),
            ("price".to_string(), "10000".to_string()),
            ("identifier".to_string(), "ord-123".to_string()),
        ];
        let json_body = pairs_to_json_object(&pairs).expect("json body");
        assert_eq!(
            json_body,
            "{\"market\":\"KRW-BTC\",\"side\":\"bid\",\"ord_type\":\"price\",\"price\":\"10000\",\"identifier\":\"ord-123\"}"
        );
    }
}
