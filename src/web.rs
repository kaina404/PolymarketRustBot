use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use crate::control::{
    BotCommand, CommandRequest, CommandResponse, CommandStatus, ControlHandle, ControlSnapshot,
    RuntimeConfigPatch,
};
use crate::ui::{DashboardHandle, DashboardState, HealthStatus, PriceDir};

const INDEX_HTML: &str = include_str!("web/index.html");

#[derive(Clone)]
pub struct WebAppState {
    dashboard: DashboardHandle,
    control: ControlHandle,
    commands: mpsc::Sender<CommandRequest>,
    admin_token: String,
}

impl WebAppState {
    pub fn new(
        dashboard: DashboardHandle,
        control: ControlHandle,
        commands: mpsc::Sender<CommandRequest>,
        admin_token: impl Into<String>,
    ) -> Self {
        Self {
            dashboard,
            control,
            commands,
            admin_token: admin_token.into(),
        }
    }
}

pub fn router(state: WebAppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/dashboard", get(dashboard_snapshot))
        .route("/api/control", get(control_snapshot))
        .route("/api/control/pause", post(pause_trading))
        .route("/api/control/resume", post(resume_trading))
        .route("/api/control/merge-now", post(run_merge_now))
        .route("/api/control/cancel-all", post(cancel_all_orders))
        .route("/api/control/shutdown", post(shutdown))
        .route("/api/control/config", patch(update_runtime_config))
        .with_state(state)
}

pub async fn serve(addr: SocketAddr, state: WebAppState) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, router(state)).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        now: Utc::now(),
    })
}

async fn dashboard_snapshot(
    State(state): State<WebAppState>,
    headers: HeaderMap,
) -> Result<Json<DashboardSnapshot>, ApiError> {
    require_auth(&headers, &state.admin_token)?;
    let control = state.control.snapshot();
    let snapshot = state
        .dashboard
        .with(|dashboard| DashboardSnapshot::from_parts(dashboard, control));
    Ok(Json(snapshot))
}

async fn control_snapshot(
    State(state): State<WebAppState>,
    headers: HeaderMap,
) -> Result<Json<ControlSnapshot>, ApiError> {
    require_auth(&headers, &state.admin_token)?;
    Ok(Json(state.control.snapshot()))
}

async fn pause_trading(
    State(state): State<WebAppState>,
    headers: HeaderMap,
) -> Result<Json<CommandResponse>, ApiError> {
    require_auth(&headers, &state.admin_token)?;
    dispatch_command(&state, BotCommand::PauseTrading).await
}

async fn resume_trading(
    State(state): State<WebAppState>,
    headers: HeaderMap,
) -> Result<Json<CommandResponse>, ApiError> {
    require_auth(&headers, &state.admin_token)?;
    dispatch_command(&state, BotCommand::ResumeTrading).await
}

async fn run_merge_now(
    State(state): State<WebAppState>,
    headers: HeaderMap,
    Json(body): Json<ConfirmBody>,
) -> Result<Json<CommandResponse>, ApiError> {
    require_auth(&headers, &state.admin_token)?;
    dispatch_command(
        &state,
        BotCommand::RunMergeNow {
            confirm: body.is_confirmed(),
        },
    )
    .await
}

async fn cancel_all_orders(
    State(state): State<WebAppState>,
    headers: HeaderMap,
    Json(body): Json<ConfirmBody>,
) -> Result<Json<CommandResponse>, ApiError> {
    require_auth(&headers, &state.admin_token)?;
    dispatch_command(
        &state,
        BotCommand::CancelAllOrders {
            confirm: body.is_confirmed(),
        },
    )
    .await
}

async fn shutdown(
    State(state): State<WebAppState>,
    headers: HeaderMap,
    Json(body): Json<ConfirmBody>,
) -> Result<Json<CommandResponse>, ApiError> {
    require_auth(&headers, &state.admin_token)?;
    dispatch_command(
        &state,
        BotCommand::Shutdown {
            confirm: body.is_confirmed(),
        },
    )
    .await
}

async fn update_runtime_config(
    State(state): State<WebAppState>,
    headers: HeaderMap,
    Json(patch): Json<RuntimeConfigPatch>,
) -> Result<Json<CommandResponse>, ApiError> {
    require_auth(&headers, &state.admin_token)?;
    dispatch_command(&state, BotCommand::UpdateRuntimeConfig { patch }).await
}

async fn dispatch_command(
    state: &WebAppState,
    command: BotCommand,
) -> Result<Json<CommandResponse>, ApiError> {
    if !command.is_confirmed() {
        return Err(ApiError::with_response(
            StatusCode::BAD_REQUEST,
            CommandResponse::rejected(format!("{} requires confirm=true", command.name())),
        ));
    }

    let (respond_to, response_rx) = oneshot::channel();
    state
        .commands
        .send(CommandRequest::new(command, respond_to))
        .await
        .map_err(|_| ApiError::service_unavailable("command processor is unavailable"))?;

    let response = tokio::time::timeout(Duration::from_secs(5), response_rx)
        .await
        .map_err(|_| ApiError::service_unavailable("command processor timed out"))?
        .map_err(|_| ApiError::service_unavailable("command processor dropped response"))?;

    let status = if response.status == CommandStatus::Rejected {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::OK
    };
    if status == StatusCode::OK {
        Ok(Json(response))
    } else {
        Err(ApiError::with_response(status, response))
    }
}

fn require_auth(headers: &HeaderMap, expected_token: &str) -> Result<(), ApiError> {
    let header_value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    validate_bearer_token(header_value, expected_token).map_err(ApiError::unauthorized)
}

pub fn validate_bearer_token(
    header_value: Option<&str>,
    expected_token: &str,
) -> Result<(), String> {
    if expected_token.trim().is_empty() {
        return Err("admin token is not configured".to_string());
    }

    let Some(header_value) = header_value else {
        return Err("missing bearer token".to_string());
    };
    let Some(token) = header_value.strip_prefix("Bearer ") else {
        return Err("authorization header must use Bearer scheme".to_string());
    };
    if constant_time_eq(token.as_bytes(), expected_token.as_bytes()) {
        Ok(())
    } else {
        Err("invalid bearer token".to_string())
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0u8, |acc, (l, r)| acc | (l ^ r))
        == 0
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ConfirmBody {
    pub confirm: bool,
}

impl ConfirmBody {
    pub fn is_confirmed(&self) -> bool {
        self.confirm
    }
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    now: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DashboardSnapshot {
    pub now: DateTime<Utc>,
    pub connected: bool,
    pub live_mode: bool,
    pub uptime: String,
    pub window_label: String,
    pub window_secs_left: u32,
    pub window_countdown: String,
    pub order_mode: String,
    pub exposure: f64,
    pub exposure_limit: f64,
    pub exposure_pct: f64,
    pub positions: u32,
    pub arb_scans: u64,
    pub session_pnl: f64,
    pub window_pnl: f64,
    pub last_trade_pnl: f64,
    pub total_trades: u32,
    pub successful_trades: u32,
    pub win_rate: f64,
    pub best_trade: f64,
    pub last_trade_symbol: String,
    pub merge_status: String,
    pub markets: Vec<MarketSnapshot>,
    pub services: Vec<ServiceSnapshot>,
    pub events: Vec<String>,
    pub control: ControlSnapshot,
}

impl DashboardSnapshot {
    pub fn from_parts(state: &DashboardState, control: ControlSnapshot) -> Self {
        Self {
            now: Utc::now(),
            connected: state.connected,
            live_mode: state.live_mode,
            uptime: state.uptime(),
            window_label: state.window_label.clone(),
            window_secs_left: state.window_secs_left,
            window_countdown: state.window_countdown(),
            order_mode: state.order_mode.clone(),
            exposure: state.exposure,
            exposure_limit: state.exposure_limit,
            exposure_pct: state.exposure_pct(),
            positions: state.positions,
            arb_scans: state.arb_scans,
            session_pnl: state.session_pnl,
            window_pnl: state.window_pnl,
            last_trade_pnl: state.last_trade_pnl,
            total_trades: state.total_trades,
            successful_trades: state.successful_trades,
            win_rate: state.win_rate(),
            best_trade: state.best_trade,
            last_trade_symbol: state.last_trade_symbol.clone(),
            merge_status: state.merge_status.clone(),
            markets: state
                .markets
                .iter()
                .map(|market| MarketSnapshot {
                    symbol: market.symbol.clone(),
                    yes_price: market.yes_price,
                    no_price: market.no_price,
                    yes_dir: dir_label(market.yes_dir),
                    no_dir: dir_label(market.no_dir),
                    total_price: market.yes_price + market.no_price,
                    edge_pct: state.profit_pct(market),
                    is_arb: market.is_arb,
                    sparkline: market.sparkline.clone(),
                })
                .collect(),
            services: state
                .services
                .iter()
                .map(|service| ServiceSnapshot {
                    name: service.name,
                    status: health_label(service.status),
                    latency_ms: service.latency_ms,
                })
                .collect(),
            events: state.recent_events(30),
            control,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MarketSnapshot {
    pub symbol: String,
    pub yes_price: f64,
    pub no_price: f64,
    pub yes_dir: &'static str,
    pub no_dir: &'static str,
    pub total_price: f64,
    pub edge_pct: f64,
    pub is_arb: bool,
    pub sparkline: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ServiceSnapshot {
    pub name: &'static str,
    pub status: &'static str,
    pub latency_ms: u32,
}

fn dir_label(dir: PriceDir) -> &'static str {
    match dir {
        PriceDir::Up => "up",
        PriceDir::Down => "down",
        PriceDir::Flat => "flat",
    }
}

fn health_label(status: HealthStatus) -> &'static str {
    match status {
        HealthStatus::Ok => "ok",
        HealthStatus::Warn => "warn",
        HealthStatus::Err => "err",
    }
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    response: CommandResponse,
}

impl ApiError {
    fn unauthorized(message: String) -> Self {
        Self::with_message(StatusCode::UNAUTHORIZED, message)
    }

    fn service_unavailable(message: impl Into<String>) -> Self {
        Self::with_message(StatusCode::SERVICE_UNAVAILABLE, message)
    }

    fn with_message(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            response: CommandResponse::rejected(message),
        }
    }

    fn with_response(status: StatusCode, response: CommandResponse) -> Self {
        Self { status, response }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.status, Json(self.response)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{ControlHandle, RuntimeConfig};
    use crate::ui::DashboardState;

    #[test]
    fn bearer_token_validation_accepts_exact_bearer_value() {
        assert!(validate_bearer_token(Some("Bearer secret-token"), "secret-token").is_ok());
        assert!(validate_bearer_token(Some("Bearer wrong"), "secret-token").is_err());
        assert!(validate_bearer_token(Some("Basic secret-token"), "secret-token").is_err());
        assert!(validate_bearer_token(None, "secret-token").is_err());
    }

    #[test]
    fn confirm_body_requires_explicit_true() {
        assert!(ConfirmBody { confirm: true }.is_confirmed());
        assert!(!ConfirmBody { confirm: false }.is_confirmed());
    }

    #[test]
    fn dashboard_snapshot_contains_public_operational_state() {
        let mut state = DashboardState::new_live("GTD", 500.0);
        state.set_connected(true);
        state.set_window("btc-updown-5m", 123);
        state.set_exposure(125.0);
        state.ensure_market("BTC");
        state.update_market("BTC", 0.42, 0.55, true);

        let control = ControlHandle::new(RuntimeConfig::default());
        let snapshot = DashboardSnapshot::from_parts(&state, control.snapshot());

        assert!(snapshot.connected);
        assert_eq!(snapshot.window_label, "btc-updown-5m");
        assert_eq!(snapshot.window_secs_left, 123);
        assert_eq!(snapshot.exposure, 125.0);
        assert_eq!(snapshot.control.runtime_config.max_order_size_usdc, 100.0);
        assert_eq!(snapshot.markets.len(), 1);
        assert_eq!(snapshot.markets[0].symbol, "BTC");
        assert!(snapshot.markets[0].is_arb);
    }
}
