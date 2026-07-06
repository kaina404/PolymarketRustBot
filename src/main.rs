mod config;
mod market;
mod monitor;
mod risk;
mod trading;
mod utils;

use polypulse::control::{
    BotCommand, CommandRequest, CommandResponse, ControlHandle, RuntimeConfig,
};
use polypulse::merge;
use polypulse::positions::{get_positions, Position};
use polypulse::redeem;
use polypulse::ui::{decimal_to_f64, spawn_dashboard_thread, symbol_short, DashboardHandle};
use polypulse::web::{self, WebAppState};

use anyhow::Result;
use dashmap::DashMap;
use futures::StreamExt;
use polymarket_client_sdk::types::{Address, B256, U256};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{stdout, IsTerminal};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::market::{MarketDiscoverer, MarketInfo, MarketScheduler};
use crate::monitor::{ArbitrageDetector, OrderBookMonitor};
use crate::risk::positions::PositionTracker;
use crate::risk::{HedgeMonitor, PositionBalancer, RiskManager};
use crate::trading::{
    create_authenticated_clob_client, parse_signature_type, v1_address_to_v2, TradingExecutor,
};

/// Filter condition_ids from positions where **both YES and NO** are held; only these markets can merge. One-sided positions are skipped.
/// Data API may return outcome_index 0/1 (0=Yes, 1=No) or 1/2 (CTF index_set convention); both are supported.
fn condition_ids_with_both_sides(positions: &[Position]) -> Vec<B256> {
    let mut by_condition: HashMap<B256, HashSet<i32>> = HashMap::new();
    for p in positions {
        if p.size <= dec!(0) || !p.mergeable {
            continue;
        }
        by_condition
            .entry(p.condition_id)
            .or_default()
            .insert(p.outcome_index);
    }
    by_condition
        .into_iter()
        .filter(|(_, indices)| {
            (indices.contains(&0) && indices.contains(&1))
                || (indices.contains(&1) && indices.contains(&2))
        })
        .map(|(c, _)| c)
        .collect()
}

/// Build condition_id -> (yes_token_id, no_token_id, merge_amount) from positions for exposure deduction after successful merge.
/// Supports outcome_index 0/1 (0=Yes, 1=No) and 1/2 (CTF convention).
fn merge_info_with_both_sides(positions: &[Position]) -> HashMap<B256, (U256, U256, Decimal)> {
    // outcome_index -> (asset, size) grouped by condition
    let mut by_condition: HashMap<B256, HashMap<i32, (U256, Decimal)>> = HashMap::new();
    for p in positions {
        if p.size <= dec!(0) || !p.mergeable {
            continue;
        }
        by_condition
            .entry(p.condition_id)
            .or_default()
            .insert(p.outcome_index, (p.asset, p.size));
    }
    by_condition
        .into_iter()
        .filter_map(|(c, map)| {
            // Prefer CTF convention 1=Yes, 2=No; otherwise use 0=Yes, 1=No
            if let (Some((yes_token, yes_size)), Some((no_token, no_size))) =
                (map.get(&1).copied(), map.get(&2).copied())
            {
                return Some((c, (yes_token, no_token, yes_size.min(no_size))));
            }
            if let (Some((yes_token, yes_size)), Some((no_token, no_size))) =
                (map.get(&0).copied(), map.get(&1).copied())
            {
                return Some((c, (yes_token, no_token, yes_size.min(no_size))));
            }
            None
        })
        .collect()
}

/// Convert on-chain share amount (6 decimals) to Decimal for position tracker updates.
fn raw_shares_to_decimal(raw: U256) -> Decimal {
    Decimal::from_str(&raw.to_string()).unwrap_or(dec!(0)) / dec!(1_000_000)
}

fn runtime_config_from_config(config: &Config) -> RuntimeConfig {
    RuntimeConfig {
        max_order_size_usdc: config.max_order_size_usdc,
        arbitrage_execution_spread: config.arbitrage_execution_spread,
        stop_arbitrage_before_end_seconds: config.stop_arbitrage_before_end_seconds,
        wind_down_before_window_end_seconds: config.wind_down_before_window_end_seconds,
        min_yes_price_threshold: config.min_yes_price_threshold,
        min_no_price_threshold: config.min_no_price_threshold,
    }
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            matches!(value.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

fn web_bind_from_env() -> Result<SocketAddr> {
    let bind = env::var("WEB_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    bind.parse()
        .map_err(|e| anyhow::anyhow!("Invalid WEB_BIND value {bind:?}: {e}"))
}

fn admin_token_from_env() -> Result<String> {
    let token = env::var("ADMIN_TOKEN")
        .map_err(|_| anyhow::anyhow!("WEB_ENABLED=true requires ADMIN_TOKEN"))?;
    let token = token.trim().to_string();
    if token.is_empty() {
        Err(anyhow::anyhow!(
            "WEB_ENABLED=true requires non-empty ADMIN_TOKEN"
        ))
    } else {
        Ok(token)
    }
}

fn control_state_path_from_env() -> PathBuf {
    env::var("CONTROL_STATE_PATH")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("data/control_state.json"))
}

/// 判断错误是否为 RPC 限流（429 / rate limit），用于决定是否退避重试。
/// 免费公共节点(publicnode/allnodes)限流 1200rqs/60s，错误体形如
/// "HTTP error 429 with body: Rate limit (1200rqs/60s) reached"（注意大写）。
fn is_rate_limit_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("rate limit") || m.contains("429") || m.contains("retry in")
}

/// Run a single Merge pass for all currently mergeable YES+NO positions.
async fn run_merge_once(
    label: &str,
    proxy: Address,
    private_key: &str,
    position_tracker: Arc<PositionTracker>,
    dashboard: Option<DashboardHandle>,
) -> Result<usize> {
    const DELAY_BETWEEN_MERGES: Duration = Duration::from_secs(30);
    const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(12);

    let positions = get_positions().await?;
    let condition_ids = condition_ids_with_both_sides(&positions);
    let merge_info = merge_info_with_both_sides(&positions);

    if condition_ids.is_empty() {
        debug!("{label}: no markets with both YES+NO positions");
        if let Some(dashboard) = &dashboard {
            dashboard.with_mut(|d| d.push_event(format!("{label}: no mergeable YES+NO positions")));
        }
        return Ok(0);
    }

    info!(
        count = condition_ids.len(),
        "{label}: {} markets have both YES+NO positions",
        condition_ids.len()
    );
    if let Some(dashboard) = &dashboard {
        dashboard.with_mut(|d| {
            d.set_merge_status("running");
            d.push_event(format!("{label}: merging {} markets", condition_ids.len()));
        });
    }

    let mut merged = 0usize;
    for (i, &condition_id) in condition_ids.iter().enumerate() {
        if i > 0 {
            info!(
                "{label}: waiting 30s before merging next market ({}/{})",
                i + 1,
                condition_ids.len()
            );
            sleep(DELAY_BETWEEN_MERGES).await;
        }

        let asset_hint = merge_info
            .get(&condition_id)
            .map(|(yes_token, no_token, _)| (*yes_token, *no_token));
        let mut result = merge::merge_max(condition_id, proxy, private_key, None, asset_hint).await;
        if result.is_err() {
            let msg = result.as_ref().unwrap_err().to_string();
            if is_rate_limit_error(&msg) {
                warn!(
                    condition_id = %condition_id,
                    "RPC rate limit, waiting {}s before retry",
                    RATE_LIMIT_BACKOFF.as_secs()
                );
                sleep(RATE_LIMIT_BACKOFF).await;
                result = merge::merge_max(condition_id, proxy, private_key, None, asset_hint).await;
            }
        }

        match result {
            Ok(merge_result) => {
                merged += 1;
                info!("{label}: Merge complete | condition_id={:#x}", condition_id);
                info!("  tx={}", merge_result.tx_hash);
                let chain_amt = raw_shares_to_decimal(merge_result.merged_amount);
                if let Some((yes_token, no_token, merge_amt)) = merge_info.get(&condition_id) {
                    let deduct = chain_amt.min(*merge_amt);
                    position_tracker.update_exposure_cost(*yes_token, dec!(0), -deduct);
                    position_tracker.update_exposure_cost(*no_token, dec!(0), -deduct);
                    position_tracker.update_position(*yes_token, -deduct);
                    position_tracker.update_position(*no_token, -deduct);
                    info!(
                        "{label}: Merge deducted exposure | condition_id={:#x} | amount:{} (chain:{})",
                        condition_id, deduct, chain_amt
                    );
                }
                if let Some(dashboard) = &dashboard {
                    dashboard.with_mut(|d| {
                        d.push_event(format!(
                            "{label}: merged condition {:#x} tx {}",
                            condition_id, merge_result.tx_hash
                        ));
                    });
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("no mergeable shares") {
                    debug!(condition_id = %condition_id, "{label}: skip merge, no mergeable shares");
                } else {
                    warn!(condition_id = %condition_id, error = %e, "{label}: Merge failed");
                    if let Some(dashboard) = &dashboard {
                        dashboard.with_mut(|d| {
                            d.push_event(format!(
                                "{label}: merge failed for {condition_id:#x}: {e}"
                            ));
                        });
                    }
                }
            }
        }
        tokio::task::yield_now().await;
    }

    if let Some(dashboard) = &dashboard {
        dashboard.with_mut(|d| d.set_merge_status("idle"));
    }
    Ok(merged)
}

async fn run_command_processor(
    mut receiver: mpsc::Receiver<CommandRequest>,
    control: ControlHandle,
    dashboard: DashboardHandle,
    shutdown: Arc<AtomicBool>,
    executor: Arc<TradingExecutor>,
    config: Config,
    position_tracker: Arc<PositionTracker>,
    wind_down_in_progress: Arc<AtomicBool>,
) {
    while let Some(request) = receiver.recv().await {
        let response = handle_command(
            request.command,
            control.clone(),
            dashboard.clone(),
            shutdown.clone(),
            executor.clone(),
            config.clone(),
            position_tracker.clone(),
            wind_down_in_progress.clone(),
        )
        .await;
        let _ = request.respond_to.send(response);
    }
}

async fn handle_command(
    command: BotCommand,
    control: ControlHandle,
    dashboard: DashboardHandle,
    shutdown: Arc<AtomicBool>,
    executor: Arc<TradingExecutor>,
    config: Config,
    position_tracker: Arc<PositionTracker>,
    wind_down_in_progress: Arc<AtomicBool>,
) -> CommandResponse {
    if !command.is_confirmed() {
        return CommandResponse::rejected(format!("{} requires confirm=true", command.name()));
    }

    match command {
        BotCommand::PauseTrading => {
            match control.set_trading_paused(true, "trading paused from web console") {
                Ok(()) => {
                    dashboard.with_mut(|d| d.push_event("Trading paused from web console"));
                    CommandResponse::accepted("trading paused")
                }
                Err(e) => CommandResponse::rejected(e),
            }
        }
        BotCommand::ResumeTrading => {
            match control.set_trading_paused(false, "trading resumed from web console") {
                Ok(()) => {
                    dashboard.with_mut(|d| d.push_event("Trading resumed from web console"));
                    CommandResponse::accepted("trading resumed")
                }
                Err(e) => CommandResponse::rejected(e),
            }
        }
        BotCommand::UpdateRuntimeConfig { patch } => {
            match control.update_runtime_config(&patch, "runtime config updated from web console") {
                Ok(_) => {
                    dashboard.with_mut(|d| d.push_event("Runtime config updated from web console"));
                    CommandResponse::accepted("runtime config updated")
                }
                Err(e) => CommandResponse::rejected(e),
            }
        }
        BotCommand::RunMergeNow { .. } => {
            if wind_down_in_progress.load(Ordering::Relaxed) {
                return CommandResponse::rejected(
                    "wind-down is in progress; merge command rejected",
                );
            }
            if control.snapshot().merge_running {
                return CommandResponse::rejected("merge is already running");
            }
            let Some(proxy) = config.proxy_address else {
                return CommandResponse::rejected("POLYMARKET_PROXY_ADDRESS is required for merge");
            };

            control.set_merge_running(true);
            control.record_command("manual merge started from web console");
            dashboard.with_mut(|d| d.push_event("Manual merge started from web console"));

            let control_done = control.clone();
            let dashboard_done = dashboard.clone();
            let private_key = config.private_key.clone();
            tokio::spawn(async move {
                let result = run_merge_once(
                    "Manual merge",
                    proxy,
                    &private_key,
                    position_tracker,
                    Some(dashboard_done.clone()),
                )
                .await;
                match result {
                    Ok(count) => {
                        control_done
                            .record_command(format!("manual merge finished: {count} markets"));
                        dashboard_done.with_mut(|d| {
                            d.push_event(format!("Manual merge finished: {count} markets"));
                        });
                    }
                    Err(e) => {
                        control_done.record_error(format!("manual merge failed: {e}"));
                        dashboard_done.with_mut(|d| {
                            d.push_event(format!("Manual merge failed: {e}"));
                            d.set_merge_status("error");
                        });
                    }
                }
                control_done.set_merge_running(false);
            });

            CommandResponse::accepted("manual merge started")
        }
        BotCommand::CancelAllOrders { .. } => {
            if control.snapshot().cancel_running {
                return CommandResponse::rejected("cancel-all is already running");
            }
            control.set_cancel_running(true);
            control.record_command("cancel all orders started from web console");
            dashboard.with_mut(|d| d.push_event("Cancel all orders started from web console"));

            let control_done = control.clone();
            let dashboard_done = dashboard.clone();
            tokio::spawn(async move {
                match executor.cancel_all_orders().await {
                    Ok(_) => {
                        control_done.record_command("cancel all orders finished");
                        dashboard_done.with_mut(|d| d.push_event("Cancel all orders finished"));
                    }
                    Err(e) => {
                        control_done.record_error(format!("cancel all orders failed: {e}"));
                        dashboard_done
                            .with_mut(|d| d.push_event(format!("Cancel all orders failed: {e}")));
                    }
                }
                control_done.set_cancel_running(false);
            });

            CommandResponse::accepted("cancel all orders started")
        }
        BotCommand::Shutdown { .. } => {
            control.request_shutdown("shutdown requested from web console");
            dashboard.with_mut(|d| d.push_event("Shutdown requested from web console"));
            shutdown.store(true, Ordering::Relaxed);
            CommandResponse::accepted("shutdown requested")
        }
    }
}

/// Scheduled Merge task: every interval_minutes minutes fetch **positions**, run merge_max **serially** only for markets with both YES+NO positions,
/// skip one-sided positions; delay between each merge, retry once on RPC rate limit. After merge success, deduct position_tracker holdings and exposure.
/// Brief initial delay before first run to avoid blocking the orderbook stream by competing for runtime at startup.
async fn run_merge_task(
    interval_minutes: u64,
    proxy: Address,
    private_key: String,
    position_tracker: Arc<PositionTracker>,
    wind_down_in_progress: Arc<AtomicBool>,
    control: ControlHandle,
    dashboard: DashboardHandle,
) {
    let interval = Duration::from_secs(interval_minutes * 60);
    /// Initial delay so main loop can finish orderbook subscription and enter select! before first merge
    const INITIAL_DELAY: Duration = Duration::from_secs(10);

    // Let main loop complete get_markets, create stream and start orderbook listening before first merge
    sleep(INITIAL_DELAY).await;

    loop {
        if wind_down_in_progress.load(Ordering::Relaxed) {
            info!("Wind-down in progress, skipping merge for this round");
            sleep(interval).await;
            continue;
        }

        if control.snapshot().merge_running {
            info!("Another merge is already running, skipping scheduled merge round");
            sleep(interval).await;
            continue;
        }

        control.set_merge_running(true);
        match run_merge_once(
            "Scheduled merge",
            proxy,
            &private_key,
            position_tracker.clone(),
            Some(dashboard.clone()),
        )
        .await
        {
            Ok(count) => {
                control.record_command(format!("scheduled merge finished: {count} markets"))
            }
            Err(e) => {
                warn!(error = %e, "Failed to run scheduled merge");
                control.record_error(format!("scheduled merge failed: {e}"));
            }
        }
        control.set_merge_running(false);
        sleep(interval).await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Must set rustls default crypto provider first, otherwise reqwest/alloy etc. will panic when using TLS

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let use_tui = utils::logger::tui_enabled_from_env() && stdout().is_terminal();
    utils::logger::init_logger(use_tui)?;

    let shutdown = Arc::new(AtomicBool::new(false));

    if use_tui {
        tracing::info!(
            "Polymarket 5-minute arbitrage bot starting (dashboard mode, logs → bot.log)"
        );
    } else {
        tracing::info!("Polymarket 5-minute arbitrage bot starting");
    }

    // Load config
    let config = Config::from_env()?;
    let web_enabled = env_flag("WEB_ENABLED");
    let web_bind = if web_enabled {
        Some(web_bind_from_env()?)
    } else {
        None
    };
    let admin_token = if web_enabled {
        Some(admin_token_from_env()?)
    } else {
        None
    };
    tracing::info!(
        max_orderbook_pair_skew_ms = config.max_orderbook_pair_skew_ms,
        "Config loaded"
    );

    let order_mode = format!("{:?}", config.arbitrage_order_type);
    let dashboard = DashboardHandle::new_live(order_mode, config.risk_max_exposure_usdc);
    let control_state_path = control_state_path_from_env();
    let control = ControlHandle::with_persistence(
        runtime_config_from_config(&config),
        control_state_path.clone(),
    )
    .map_err(|err| {
        anyhow::anyhow!(
            "Failed to load control state from {}: {err}",
            control_state_path.display()
        )
    })?;
    info!(
        path = %control_state_path.display(),
        trading_paused = control.trading_paused(),
        "Control state persistence enabled"
    );
    if control.trading_paused() {
        dashboard.with_mut(|d| d.push_event("Trading paused until manual resume after startup"));
    }
    let (command_tx, command_rx) = mpsc::channel::<CommandRequest>(32);
    if use_tui {
        spawn_dashboard_thread(dashboard.arc(), shutdown.clone());
        dashboard.with_mut(|d| {
            d.push_event("🚀 Dashboard ready — hunting arbitrage opportunities");
        });
    }

    // Initialize components (currently unused, main loop disabled)
    let _discoverer = MarketDiscoverer::new(config.crypto_symbols.clone());
    let _scheduler = MarketScheduler::new(_discoverer, config.market_refresh_advance_secs);
    let _detector = ArbitrageDetector::new(config.min_profit_threshold);

    // Validate private key format
    info!("Verifying private key format...");
    use alloy::signers::local::LocalSigner;
    use std::str::FromStr;

    let _signer_test = LocalSigner::from_str(&config.private_key)
        .map_err(|e| anyhow::anyhow!("Invalid private key format: {}", e))?;
    info!("Private key format validated");

    // Initialize trading executor (requires API auth)
    info!("Initializing trading executor (requires API auth)...");
    if let Some(ref proxy) = config.proxy_address {
        info!(
            proxy_address = %proxy,
            signature_type = %config.signature_type,
            "Using funder address for V2 CLOB (default SIGNATURE_TYPE=Poly1271 for deposit wallet; use Proxy only for legacy Magic proxy)"
        );
    } else {
        info!("Using EOA signature type (direct trading)");
    }
    info!("Note: A 'Could not create api key' warning is normal. The SDK tries to create a new API key first; if it fails, it will use the derived method and auth will still succeed.");
    info!(clob_url = %config.clob_api_url, signature_type = %config.signature_type, "CLOB V2 client config");

    let v2_proxy = config.proxy_address.map(v1_address_to_v2);
    let sig_type = parse_signature_type(&config.signature_type);

    let clob_client = match create_authenticated_clob_client(
        &config.private_key,
        &config.clob_api_url,
        v2_proxy,
        sig_type,
    )
    .await
    {
        Ok(client) => {
            info!("CLOB V2 client authenticated");
            client
        }
        Err(e) => {
            error!(error = %e, "CLOB V2 authentication failed! Cannot continue.");
            error!("Please check:");
            error!("  1. POLYMARKET_PRIVATE_KEY is correctly set");
            error!("  2. Private key format (64-char hex, no 0x prefix)");
            error!("  3. Network connectivity");
            error!("  4. Polymarket API availability");
            error!("  5. CLOB_API_URL (use https://clob.polymarket.com, not clob-v2 host)");
            return Err(anyhow::anyhow!("Authentication failed, exiting: {}", e));
        }
    };

    let executor = Arc::new(TradingExecutor::from_client(
        clob_client.clone(),
        config.private_key.clone(),
        config.max_order_size_usdc,
        config.slippage,
        config.arbitrage_validate_slippage_adjusted_total,
        config.gtd_expiration_secs,
        config.arbitrage_order_type.clone(),
        config.arbitrage_hedge_grace_secs,
    ));
    let arbitrage_slippage = [
        Decimal::try_from(config.slippage[0]).unwrap_or(dec!(0.0)),
        Decimal::try_from(config.slippage[1]).unwrap_or(dec!(0.01)),
    ];
    let arbitrage_min_available_shares =
        Decimal::try_from(config.arbitrage_min_available_shares).unwrap_or(dec!(5.0));
    let arbitrage_order_size_ratio =
        Decimal::try_from(config.arbitrage_order_size_ratio).unwrap_or(dec!(1.0));
    let arbitrage_validate_slippage_adjusted_total =
        config.arbitrage_validate_slippage_adjusted_total;

    let _risk_manager = Arc::new(RiskManager::new(clob_client.clone(), &config));

    // Create hedge monitor (pass PositionTracker Arc for exposure updates)
    // Hedge strategy is currently disabled but hedge_monitor kept for future use
    let position_tracker = _risk_manager.position_tracker();
    let _hedge_monitor = HedgeMonitor::new(
        clob_client.clone(),
        config.private_key.clone(),
        config.proxy_address.clone(),
        position_tracker,
    );

    // Verify auth actually succeeded - try a simple API call
    info!("Verifying authentication (via API call test)...");
    match executor.verify_authentication().await {
        Ok(_) => {
            info!("✅ Auth verified, API calls OK");
            dashboard.with_mut(|d| d.push_event("✅ CLOB authenticated — ready to trade"));
        }
        Err(e) => {
            error!(error = %e, "❌ Auth verification failed! authenticate() did not error but API calls fail.");
            error!("This indicates auth did not actually succeed. Possible causes:");
            error!("  1. API key creation failed ('Could not create api key' warning)");
            error!("  2. Account may not be registered on Polymarket");
            error!("  3. Account may be restricted or suspended");
            error!("  4. Network issues");
            error!("Exiting. Please fix authentication before running again.");
            return Err(anyhow::anyhow!("Auth verification failed: {}", e));
        }
    }

    info!("✅ All components initialized, auth verified");

    // Create position balancer
    let position_balancer = Arc::new(PositionBalancer::new(
        clob_client.clone(),
        _risk_manager.position_tracker(),
        &config,
    ));

    // Scheduled position sync: every N seconds fetch latest positions from API, overwrite local cache
    let position_sync_interval = config.position_sync_interval_secs;
    if position_sync_interval > 0 {
        let position_tracker_sync = _risk_manager.position_tracker();
        tokio::spawn(async move {
            let interval = Duration::from_secs(position_sync_interval);
            loop {
                match position_tracker_sync.sync_from_api().await {
                    Ok(_) => {
                        // Positions printed in sync_from_api
                    }
                    Err(e) => {
                        warn!(error = %e, "Position sync failed, will retry next loop");
                    }
                }
                sleep(interval).await;
            }
        });
        info!(
            interval_secs = position_sync_interval,
            "Started position sync task, every {}s fetch latest positions from API",
            position_sync_interval
        );
    } else {
        warn!("POSITION_SYNC_INTERVAL_SECS=0, position sync disabled");
    }

    // Scheduled position balance: every N seconds check positions and orders, cancel excess orders
    // Note: balance task runs in main loop as it needs market mapping
    let balance_interval = config.position_balance_interval_secs;
    if balance_interval > 0 {
        info!(
            interval_secs = balance_interval,
            "Position balance task runs in main loop every {}s", balance_interval
        );
    } else {
        info!("Position balance not enabled (POSITION_BALANCE_INTERVAL_SECS=0)");
    }

    // Wind-down in progress flag: scheduled merge checks and skips to avoid race with wind-down merge
    let wind_down_in_progress = Arc::new(AtomicBool::new(false));

    tokio::spawn(run_command_processor(
        command_rx,
        control.clone(),
        dashboard.clone(),
        shutdown.clone(),
        executor.clone(),
        config.clone(),
        _risk_manager.position_tracker().clone(),
        wind_down_in_progress.clone(),
    ));

    if let (Some(bind), Some(admin_token)) = (web_bind, admin_token) {
        let web_state = WebAppState::new(
            dashboard.clone(),
            control.clone(),
            command_tx.clone(),
            admin_token,
        );
        tokio::spawn(async move {
            if let Err(e) = web::serve(bind, web_state).await {
                error!(error = %e, "Web control console stopped");
            }
        });
        info!(bind = %bind, "Web control console enabled");
        dashboard.with_mut(|d| d.push_event(format!("Web control console listening on {bind}")));
    } else {
        info!("Web control console disabled (WEB_ENABLED=false)");
    }

    // Minimum interval between two arbitrage trades
    const MIN_TRADE_INTERVAL: Duration = Duration::from_secs(3);
    let last_trade_time: Arc<tokio::sync::Mutex<Option<Instant>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    // Scheduled Merge: every N minutes run merge by positions, only for markets with both YES+NO
    let merge_interval = config.merge_interval_minutes;
    if merge_interval > 0 {
        if let Some(proxy) = config.proxy_address {
            let private_key = config.private_key.clone();
            let position_tracker = _risk_manager.position_tracker().clone();
            let wind_down_flag = wind_down_in_progress.clone();
            let control_merge = control.clone();
            let dashboard_merge = dashboard.clone();
            tokio::spawn(async move {
                run_merge_task(
                    merge_interval,
                    proxy,
                    private_key,
                    position_tracker,
                    wind_down_flag,
                    control_merge,
                    dashboard_merge,
                )
                .await;
            });
            info!(
                interval_minutes = merge_interval,
                "Started scheduled Merge task, every {} minutes (YES+NO both positions only)",
                merge_interval
            );
        } else {
            warn!("MERGE_INTERVAL_MINUTES={} but POLYMARKET_PROXY_ADDRESS not set, scheduled Merge disabled", merge_interval);
        }
    } else {
        info!("Scheduled Merge not enabled (MERGE_INTERVAL_MINUTES=0). To enable, set MERGE_INTERVAL_MINUTES in .env (e.g. 5 or 15)");
    }

    // Main loop enabled, start monitoring and trading
    #[allow(unreachable_code)]
    loop {
        if shutdown.load(Ordering::Relaxed) {
            tracing::info!("Shutdown requested from dashboard");
            return Ok(());
        }

        // Fetch markets for current window immediately, or wait for next window on failure
        let markets = match _scheduler.get_markets_immediately_or_wait().await {
            Ok(markets) => markets,
            Err(e) => {
                error!(error = %e, "Failed to fetch markets");
                sleep(Duration::from_secs(60)).await;
                continue;
            }
        };

        if markets.is_empty() {
            warn!("No markets found, skipping current window");
            continue;
        }

        // New round: reset risk exposure so this round accumulates from 0
        _risk_manager.position_tracker().reset_exposure();

        dashboard.with_mut(|d| {
            d.window_pnl = 0.0;
            d.markets.clear();
        });

        // Initialize orderbook monitor
        let mut monitor =
            OrderBookMonitor::with_max_pair_skew_ms(config.max_orderbook_pair_skew_ms);

        // Subscribe to all markets
        for market in &markets {
            dashboard.with_mut(|d| d.ensure_market(symbol_short(&market.crypto_symbol)));
            if let Err(e) = monitor.subscribe_market(market) {
                error!(error = %e, market_id = %market.market_id, "Failed to subscribe to market");
            }
        }

        // Create orderbook stream
        let mut stream = match monitor.create_orderbook_stream() {
            Ok(stream) => stream,
            Err(e) => {
                error!(error = %e, "Failed to create orderbook stream");
                continue;
            }
        };

        info!(market_count = markets.len(), "Monitoring orderbook");

        let window_label = markets
            .first()
            .map(|m| {
                format!(
                    "{}-updown-5m",
                    symbol_short(&m.crypto_symbol).to_lowercase()
                )
            })
            .unwrap_or_else(|| "updown-5m".to_string());

        dashboard.with_mut(|d| {
            d.set_window(&window_label, 300);
            d.set_connected(true);
            d.push_event(format!(
                "📡 Live — {} markets in {}",
                markets.len(),
                window_label
            ));
        });

        // Record current window timestamp for cycle switch and wind-down detection
        use crate::market::discoverer::FIVE_MIN_SECS;
        use chrono::Utc;
        let current_window_timestamp =
            MarketDiscoverer::calculate_current_window_timestamp(Utc::now());
        let window_end =
            chrono::DateTime::from_timestamp(current_window_timestamp + FIVE_MIN_SECS, 0)
                .unwrap_or_else(|| Utc::now());
        let mut wind_down_done = false;

        // Create market_id -> market info mapping
        let market_map: HashMap<B256, &MarketInfo> =
            markets.iter().map(|m| (m.market_id, m)).collect();

        // Create market mapping (condition_id -> (yes_token_id, no_token_id)) for position balance
        let market_token_map: HashMap<B256, (U256, U256)> = markets
            .iter()
            .map(|m| (m.market_id, (m.yes_token_id, m.no_token_id)))
            .collect();

        // Create position balance timer
        let balance_interval = config.position_balance_interval_secs;
        // Only start balancing this many seconds before window end; 0 = whole window.
        let balance_start_secs = config.position_balance_start_before_end_seconds;
        let mut balance_timer = if balance_interval > 0 {
            let mut timer = tokio::time::interval(Duration::from_secs(balance_interval));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            timer.tick().await; // Fire first tick immediately
            Some(timer)
        } else {
            None
        };

        // Record last best-ask per market for direction (↑↓) calc (single HashMap read/write, no perf impact)
        let last_prices: DashMap<B256, (Decimal, Decimal)> = DashMap::new();

        // Monitor orderbook updates
        loop {
            // Wind-down check: run once when <= N seconds to window end (stay in loop until natural switch by new-window detection)
            let runtime_config = control.runtime_config();
            if runtime_config.wind_down_before_window_end_seconds > 0 && !wind_down_done {
                let now = Utc::now();
                let seconds_until_end = (window_end - now).num_seconds();
                let threshold_seconds = runtime_config.wind_down_before_window_end_seconds as i64;
                if seconds_until_end <= threshold_seconds {
                    info!(
                        "🛑 Wind-down triggered | {}s until window end",
                        seconds_until_end
                    );
                    wind_down_done = true;
                    wind_down_in_progress.store(true, Ordering::Relaxed);

                    // Wind-down runs in separate task to avoid blocking orderbook; 30s between merges per market
                    let executor_wd = executor.clone();
                    let config_wd = config.clone();
                    let risk_manager_wd = _risk_manager.clone();
                    let wind_down_flag = wind_down_in_progress.clone();
                    let control_wd = control.clone();
                    let dashboard_wd = dashboard.clone();
                    tokio::spawn(async move {
                        const MERGE_INTERVAL: Duration = Duration::from_secs(30);

                        // 1. Cancel all orders
                        control_wd.set_cancel_running(true);
                        if let Err(e) = executor_wd.cancel_all_orders().await {
                            warn!(error = %e, "Wind-down: failed to cancel all orders, continuing with Merge and sell");
                            control_wd.record_error(format!("wind-down cancel failed: {e}"));
                        } else {
                            info!("✅ Wind-down: all orders cancelled");
                            dashboard_wd
                                .with_mut(|d| d.push_event("Wind-down: all orders cancelled"));
                        }
                        control_wd.set_cancel_running(false);

                        // Wait 10s after cancel before Merge so recent fills can settle on-chain
                        const DELAY_AFTER_CANCEL: Duration = Duration::from_secs(10);
                        sleep(DELAY_AFTER_CANCEL).await;

                        // 2. Merge both sides (30s between markets) and update exposure
                        control_wd.set_merge_running(true);
                        let position_tracker = risk_manager_wd.position_tracker();
                        let mut did_any_merge = false;
                        if let Some(proxy) = config_wd.proxy_address {
                            match get_positions().await {
                                Ok(positions) => {
                                    let condition_ids = condition_ids_with_both_sides(&positions);
                                    let merge_info = merge_info_with_both_sides(&positions);
                                    let n = condition_ids.len();
                                    for (i, condition_id) in condition_ids.iter().enumerate() {
                                        let asset_hint = merge_info.get(condition_id).map(
                                            |(yes_token, no_token, _)| (*yes_token, *no_token),
                                        );
                                        match merge::merge_max(
                                            *condition_id,
                                            proxy,
                                            &config_wd.private_key,
                                            None,
                                            asset_hint,
                                        )
                                        .await
                                        {
                                            Ok(merge_result) => {
                                                did_any_merge = true;
                                                info!(
                                                    "✅ Wind-down: Merge done | condition_id={:#x} | tx={}",
                                                    condition_id, merge_result.tx_hash
                                                );
                                                let chain_amt = raw_shares_to_decimal(
                                                    merge_result.merged_amount,
                                                );
                                                if let Some((yes_token, no_token, merge_amt)) =
                                                    merge_info.get(condition_id)
                                                {
                                                    let deduct = chain_amt.min(*merge_amt);
                                                    position_tracker.update_exposure_cost(
                                                        *yes_token,
                                                        dec!(0),
                                                        -deduct,
                                                    );
                                                    position_tracker.update_exposure_cost(
                                                        *no_token,
                                                        dec!(0),
                                                        -deduct,
                                                    );
                                                    position_tracker
                                                        .update_position(*yes_token, -deduct);
                                                    position_tracker
                                                        .update_position(*no_token, -deduct);
                                                    info!(
                                                        "💰 Wind-down: Merge deducted exposure | condition_id={:#x} | amount:{} (chain:{})",
                                                        condition_id, deduct, chain_amt
                                                    );
                                                }
                                                dashboard_wd.with_mut(|d| {
                                                    d.push_event(format!(
                                                        "Wind-down: merged {condition_id:#x} tx {}",
                                                        merge_result.tx_hash
                                                    ));
                                                });
                                            }
                                            Err(e) => {
                                                warn!(condition_id = %condition_id, error = %e, "Wind-down: Merge failed");
                                                control_wd.record_error(format!(
                                                    "wind-down merge failed: {e}"
                                                ));
                                            }
                                        }
                                        // Wait 30s after each market merge before next, for on-chain settlement
                                        if i + 1 < n {
                                            info!("Wind-down: waiting 30s before next merge");
                                            sleep(MERGE_INTERVAL).await;
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!(error = %e, "Wind-down: failed to get positions, skipping Merge");
                                }
                            }
                        } else {
                            warn!("Wind-down: POLYMARKET_PROXY_ADDRESS not set, skipping Merge");
                        }
                        control_wd.set_merge_running(false);

                        // If merge ran, wait 30s before selling one-sided legs for on-chain; else no wait
                        if did_any_merge {
                            sleep(MERGE_INTERVAL).await;
                        }

                        // 3. Redeem resolved positions from prior rounds so funds are not left
                        //    locked. Resolved-market tokens can't be merged or sold on the CLOB;
                        //    only redemption converts them back to collateral.
                        if let Some(proxy) = config_wd.proxy_address {
                            match get_positions().await {
                                Ok(positions) => {
                                    let mut redeemed: HashSet<B256> = HashSet::new();
                                    // Only redeem winning positions. On a resolved market the losing
                                    // outcome has cur_price==0 (zero payout): redeeming it burns
                                    // worthless tokens, returns no collateral, and only wastes gas —
                                    // and reverts with "execution reverted, data: 0x" when the EOA's
                                    // POL is low relative to the estimateGas fee cap (the node caps
                                    // the gas allowance at balance/maxFeePerGas; below the redeem's
                                    // gas the Safe inner call runs out of gas and reverts GS013).
                                    // Skip zero-value positions; winners have cur_price>0.
                                    let skipped_worthless = positions
                                        .iter()
                                        .filter(|p| {
                                            p.redeemable
                                                && p.size > dec!(0)
                                                && p.cur_price <= dec!(0)
                                        })
                                        .count();
                                    if skipped_worthless > 0 {
                                        info!(
                                            "Wind-down: skipped {skipped_worthless} resolved losing positions (payout 0, nothing to redeem)"
                                        );
                                    }
                                    for pos in positions.iter().filter(|p| {
                                        p.redeemable && p.size > dec!(0) && p.cur_price > dec!(0)
                                    }) {
                                        // pUSD redemption (default) redeems the whole condition at
                                        // once, so redeem each condition_id only once per round.
                                        if !redeemed.insert(pos.condition_id) {
                                            continue;
                                        }
                                        let mut result = redeem::redeem_one(
                                            pos.condition_id,
                                            pos.negative_risk,
                                            proxy,
                                            &config_wd.private_key,
                                            None,
                                            None,
                                            None,
                                            &[],
                                        )
                                        .await;
                                        // 免费 RPC 限流(429)时退避重试一次；仍失败则本轮跳过，
                                        // 下一轮 wind-down 会再次尝试（持仓仍 redeemable）。
                                        if matches!(&result, Err(e) if is_rate_limit_error(&e.to_string()))
                                        {
                                            warn!(
                                                condition_id = %pos.condition_id,
                                                "Wind-down: RPC 限流，等待 12s 后重试 redeem"
                                            );
                                            sleep(Duration::from_secs(12)).await;
                                            result = redeem::redeem_one(
                                                pos.condition_id,
                                                pos.negative_risk,
                                                proxy,
                                                &config_wd.private_key,
                                                None,
                                                None,
                                                None,
                                                &[],
                                            )
                                            .await;
                                        }
                                        match result {
                                            Ok(tx) => {
                                                info!(
                                                    "✅ Wind-down: redeemed | condition_id={:#x} | tx={}",
                                                    pos.condition_id, tx
                                                );
                                                dashboard_wd.with_mut(|d| {
                                                    d.push_event(format!(
                                                        "Wind-down: redeemed {:#x} tx {}",
                                                        pos.condition_id, tx
                                                    ))
                                                });
                                            }
                                            Err(e) => {
                                                warn!(condition_id = %pos.condition_id, error = %e, "Wind-down: redeem failed");
                                                control_wd.record_error(format!(
                                                    "wind-down redeem failed: {e}"
                                                ));
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!(error = %e, "Wind-down: failed to get positions, skipping redeem");
                                }
                            }
                        }

                        // 4. Market-sell remaining one-sided positions
                        let wind_down_sell_price =
                            Decimal::try_from(config_wd.wind_down_sell_price).unwrap_or(dec!(0.01));
                        match get_positions().await {
                            Ok(positions) => {
                                for pos in positions.iter().filter(|p| p.size > dec!(0)) {
                                    // Resolved markets are no longer tradeable on the CLOB;
                                    // submitting a sell returns 400 "invalid token id". These
                                    // should be redeemed, not sold — skip them here.
                                    if pos.redeemable {
                                        debug!(token_id = %pos.asset, size = %pos.size, "Wind-down: position resolved (redeemable), skip sell");
                                        continue;
                                    }
                                    let size_floor = (pos.size * dec!(100)).floor() / dec!(100);
                                    if size_floor < dec!(0.01) {
                                        debug!(token_id = %pos.asset, size = %pos.size, "Wind-down: position too small, skip sell");
                                        continue;
                                    }
                                    if let Err(e) = executor_wd
                                        .sell_at_price(pos.asset, wind_down_sell_price, size_floor)
                                        .await
                                    {
                                        warn!(token_id = %pos.asset, size = %pos.size, error = %e, "Wind-down: failed to sell one-sided leg");
                                    } else {
                                        info!("✅ Wind-down: sell order placed | token_id={:#x} | amount:{} | price:{:.4}", pos.asset, size_floor, wind_down_sell_price);
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, "Wind-down: failed to get positions, skipping sell");
                            }
                        }

                        info!("🛑 Wind-down complete, continuing to monitor until window end");
                        control_wd.record_command("wind-down complete");
                        dashboard_wd.with_mut(|d| d.push_event("Wind-down complete"));
                        wind_down_flag.store(false, Ordering::Relaxed);
                    });
                }
            }

            tokio::select! {
                // Handle orderbook updates
                book_result = stream.next() => {
                    match book_result {
                        Some(Ok(book)) => {
                            // Process orderbook update (book will be moved)
                            if let Some(pair) = monitor.handle_book_update(book) {
                                // Note: asks last element is best ask
                                let yes_best_ask = pair.yes_book.asks.last().map(|a| (a.price, a.size));
                                let no_best_ask = pair.no_book.asks.last().map(|a| (a.price, a.size));
                                let total_ask_price = yes_best_ask.and_then(|(p, _)| no_best_ask.map(|(np, _)| p + np));

                                let market_id = pair.market_id;
                                // Compare with prev tick for direction (↑ up ↓ down − flat), no arrow on first
                                let (yes_dir, no_dir) = match (yes_best_ask, no_best_ask) {
                                    (Some((yp, _)), Some((np, _))) => {
                                        let prev = last_prices.get(&market_id).map(|r| (r.0, r.1));
                                        let (y_dir, n_dir) = prev
                                            .map(|(ly, ln)| (
                                                if yp > ly { "↑" } else if yp < ly { "↓" } else { "−" },
                                                if np > ln { "↑" } else if np < ln { "↓" } else { "−" },
                                            ))
                                            .unwrap_or(("", ""));
                                        last_prices.insert(market_id, (yp, np));
                                        (y_dir, n_dir)
                                    }
                                    _ => ("", ""),
                                };

                                let market_info = market_map.get(&pair.market_id);
                                let market_title = market_info.map(|m| m.title.as_str()).unwrap_or("unknown market");
                                let market_symbol = market_info.map(|m| m.crypto_symbol.as_str()).unwrap_or("");
                                let market_display = if !market_symbol.is_empty() {
                                    format!("{} prediction market", market_symbol)
                                } else {
                                    market_title.to_string()
                                };

                                let (prefix, spread_info) = total_ask_price
                                    .map(|t| {
                                        if t < dec!(1.0) {
                                            let profit_pct = (dec!(1.0) - t) * dec!(100.0);
                                            ("🚨 Arbitrage", format!("total:{:.4} profit:{:.2}%", t, profit_pct))
                                        } else {
                                            ("📊", format!("total:{:.4} (no arb)", t))
                                        }
                                    })
                                    .unwrap_or_else(|| ("📊", "no data".to_string()));

                                // Direction arrows shown only for arbitrage
                                let is_arbitrage = prefix == "🚨 Arbitrage";
                                let yes_info = yes_best_ask
                                    .map(|(p, s)| {
                                        if is_arbitrage && !yes_dir.is_empty() {
                                            format!("Yes:{:.4} size:{} {}", p, s, yes_dir)
                                        } else {
                                            format!("Yes:{:.4} size:{}", p, s)
                                        }
                                    })
                                    .unwrap_or_else(|| "Yes:none".to_string());
                                let no_info = no_best_ask
                                    .map(|(p, s)| {
                                        if is_arbitrage && !no_dir.is_empty() {
                                            format!("No:{:.4} size:{} {}", p, s, no_dir)
                                        } else {
                                            format!("No:{:.4} size:{}", p, s)
                                        }
                                    })
                                    .unwrap_or_else(|| "No:none".to_string());

                                if let (Some((yp, _)), Some((np, _))) = (yes_best_ask, no_best_ask) {
                                    let sym = symbol_short(market_symbol);
                                    let is_arb = total_ask_price
                                        .map(|t| t < dec!(1.0))
                                        .unwrap_or(false);
                                    dashboard.with_mut(|d| {
                                        d.update_market(
                                            &sym,
                                            decimal_to_f64(yp),
                                            decimal_to_f64(np),
                                            is_arb,
                                        );
                                    });
                                }
                                if !use_tui {
                                    info!(
                                        "{} {} | {} | {} | {}",
                                        prefix,
                                        market_display,
                                        yes_info,
                                        no_info,
                                        spread_info
                                    );
                                }

                                // Keep structured debug log (optional)
                                debug!(
                                    market_id = %pair.market_id,
                                    yes_token = %pair.yes_book.asset_id,
                                    no_token = %pair.no_book.asset_id,
                                    "Orderbook pair details"
                                );

                                // Detect arbitrage (monitoring: execute only when total <= 1 - execution spread)
                                use rust_decimal::Decimal;
                                let runtime_config = control.runtime_config();
                                let execution_threshold = dec!(1.0) - Decimal::try_from(runtime_config.arbitrage_execution_spread)
                                    .unwrap_or(dec!(0.01));
                                if let Some(total_price) = total_ask_price {
                                    if total_price <= execution_threshold {
                                        if let Some(opp) = _detector.check_arbitrage(
                                            &pair.yes_book,
                                            &pair.no_book,
                                            &pair.market_id,
                                        ) {
                                            if control.trading_paused() {
                                                debug!(
                                                    "Trading paused, skip arbitrage | market:{}",
                                                    market_display
                                                );
                                                continue;
                                            }

                                            // Check YES price threshold
                                            if runtime_config.min_yes_price_threshold > 0.0 {
                                                use rust_decimal::Decimal;
                                                let min_yes_price_decimal =
                                                    Decimal::try_from(
                                                        runtime_config.min_yes_price_threshold,
                                                    )
                                                    .unwrap_or(dec!(0.0));
                                                if opp.yes_ask_price < min_yes_price_decimal {
                                                    debug!(
                                                        "⏸️ YES price below threshold, skip arbitrage | market:{} | YES:{:.4} | threshold:{:.4}",
                                                        market_display,
                                                        opp.yes_ask_price,
                                                        runtime_config.min_yes_price_threshold
                                                    );
                                                    continue; // Skip this arbitrage
                                                }
                                            }

                                            // Check NO price threshold
                                            if runtime_config.min_no_price_threshold > 0.0 {
                                                use rust_decimal::Decimal;
                                                let min_no_price_decimal =
                                                    Decimal::try_from(
                                                        runtime_config.min_no_price_threshold,
                                                    )
                                                    .unwrap_or(dec!(0.0));
                                                if opp.no_ask_price < min_no_price_decimal {
                                                    debug!(
                                                        "⏸️ NO price below threshold, skip arbitrage | market:{} | NO:{:.4} | threshold:{:.4}",
                                                        market_display,
                                                        opp.no_ask_price,
                                                        runtime_config.min_no_price_threshold
                                                    );
                                                    continue; // Skip this arbitrage
                                                }
                                            }

                                            // Check if near market end (if stop time configured)
                                            if runtime_config.stop_arbitrage_before_end_seconds > 0 {
                                                if let Some(market_info) = market_map.get(&pair.market_id) {
                                                    use chrono::Utc;
                                                    let now = Utc::now();
                                                    let time_until_end =
                                                        market_info.end_date.signed_duration_since(now);
                                                    let seconds_until_end = time_until_end.num_seconds();
                                                    let threshold_seconds =
                                                        runtime_config.stop_arbitrage_before_end_seconds
                                                            as i64;

                                                    if seconds_until_end <= threshold_seconds {
                                                        debug!(
                                                            "⏰ Near market end, skip arbitrage | market:{} | seconds to end:{} | stop threshold:{}s",
                                                            market_display,
                                                            seconds_until_end,
                                                            runtime_config.stop_arbitrage_before_end_seconds
                                                        );
                                                        continue; // Skip this arbitrage
                                                    }
                                                }
                                            }

                                            // Calculate order cost (USD)
                                            // Use actual available size from arb, cap it, then submit only the configured ratio.
                                            use rust_decimal::Decimal;
                                            let max_order_size =
                                                Decimal::try_from(runtime_config.max_order_size_usdc)
                                                    .unwrap_or(dec!(100.0));
                                            let available_size = TradingExecutor::capped_order_size(
                                                opp.yes_size,
                                                opp.no_size,
                                                max_order_size,
                                            );
                                            if available_size < arbitrage_min_available_shares {
                                                debug!(
                                                    "⏭️ Available shares below configured minimum, skip arbitrage | market:{} | available:{} | minimum:{}",
                                                    market_display,
                                                    available_size,
                                                    arbitrage_min_available_shares
                                                );
                                                continue; // Skip this arbitrage
                                            }
                                            let order_size = TradingExecutor::apply_order_size_ratio(
                                                available_size,
                                                arbitrage_order_size_ratio,
                                            );
                                            let yes_limit_price = TradingExecutor::limit_price_with_slippage(
                                                opp.yes_ask_price,
                                                &yes_dir,
                                                arbitrage_slippage,
                                            );
                                            let no_limit_price = TradingExecutor::limit_price_with_slippage(
                                                opp.no_ask_price,
                                                &no_dir,
                                                arbitrage_slippage,
                                            );
                                            let total_limit_price = yes_limit_price + no_limit_price;
                                            let threshold_total_price =
                                                TradingExecutor::threshold_total_price(
                                                    opp.yes_ask_price,
                                                    opp.no_ask_price,
                                                    &yes_dir,
                                                    &no_dir,
                                                    arbitrage_slippage,
                                                    arbitrage_validate_slippage_adjusted_total,
                                                );
                                            if threshold_total_price > execution_threshold {
                                                let threshold_basis =
                                                    if arbitrage_validate_slippage_adjusted_total {
                                                        "slippage-adjusted"
                                                    } else {
                                                        "raw"
                                                    };
                                                debug!(
                                                    "⏭️ {} total above threshold, skip arbitrage | market:{} | total:{:.4} | threshold:{:.4} | YES:{:.4} | NO:{:.4}",
                                                    threshold_basis,
                                                    market_display,
                                                    threshold_total_price,
                                                    execution_threshold,
                                                    yes_limit_price,
                                                    no_limit_price
                                                );
                                                continue; // Skip this arbitrage
                                            }
                                            let yes_cost = yes_limit_price * order_size;
                                            let no_cost = no_limit_price * order_size;
                                            let total_cost = yes_cost + no_cost;

                                            // Skip if buyable shares < 5 (Polymarket min order size); avoids API 400 "Size lower than the minimum: 5"
                                            if order_size < dec!(5) {
                                                debug!(
                                                    "⏭️ Buyable shares below minimum (5), skip arbitrage | market:{} | available:{} | ratio:{} | size:{} | cost:{:.2} USD",
                                                    market_display,
                                                    available_size,
                                                    arbitrage_order_size_ratio,
                                                    order_size,
                                                    total_cost
                                                );
                                                continue; // Skip this arbitrage
                                            }

                                            // Check risk exposure limit
                                            let position_tracker = _risk_manager.position_tracker();
                                            let current_exposure = position_tracker.calculate_exposure();

                                            if position_tracker.would_exceed_limit(yes_cost, no_cost) {
                                                warn!(
                                                    "�� ️ Risk exposure limit exceeded, skip arbitrage | market:{} | exposure:{:.2} USD | order cost:{:.2} USD | limit:{:.2} USD",
                                                    market_display,
                                                    current_exposure,
                                                    total_cost,
                                                    position_tracker.max_exposure()
                                                );
                                                continue; // Skip this arbitrage
                                            }

                                            // Check position balance (local cache, zero latency)
                                            if position_balancer.should_skip_arbitrage(opp.yes_token_id, opp.no_token_id) {
                                                warn!(
                                                    "�� ️ Positions severely imbalanced, skip arbitrage | market:{}",
                                                    market_display
                                                );
                                                continue; // Skip this arbitrage
                                            }

                                            // Check trade interval: min 3s between trades
                                            {
                                                let mut guard = last_trade_time.lock().await;
                                                let now = Instant::now();
                                                if let Some(last) = *guard {
                                                    if now.saturating_duration_since(last) < MIN_TRADE_INTERVAL {
                                                        let elapsed = now.saturating_duration_since(last).as_secs_f32();
                                                        debug!(
                                                            "⏱️ Trade interval < 3s, skip | market:{} | since last:{}s",
                                                            market_display,
                                                            elapsed
                                                        );
                                                        continue; // Skip this arb
                                                    }
                                                }
                                                *guard = Some(now);
                                            }

                                            let trade_sym = symbol_short(market_symbol);
                                            let execution_profit_percentage =
                                                (dec!(1.0) - total_limit_price) * dec!(100.0);
                                            let profit_usd = decimal_to_f64(
                                                (dec!(1.0) - total_limit_price) * order_size,
                                            );
                                            dashboard.with_mut(|d| {
                                                d.set_exposure(decimal_to_f64(current_exposure));
                                                d.record_trade_attempt(
                                                    &trade_sym,
                                                    decimal_to_f64(execution_profit_percentage),
                                                    decimal_to_f64(order_size),
                                                    decimal_to_f64(total_cost),
                                                );
                                            });
                                            if !use_tui {
                                                info!(
                                                    "⚡ Execute arbitrage | market:{} | profit:{:.2}% | size:{} | cost:{:.2} USD | exposure:{:.2} USD",
                                                    market_display,
                                                    execution_profit_percentage,
                                                    order_size,
                                                    total_cost,
                                                    current_exposure
                                                );
                                            }
                                            // Simplified exposure: add on arb execution regardless of fill
                                            let _pt = _risk_manager.position_tracker();
                                            _pt.update_exposure_cost(opp.yes_token_id, yes_limit_price, order_size);
                                            _pt.update_exposure_cost(opp.no_token_id, no_limit_price, order_size);

                                            // Arb execution: run whenever total <= threshold, direction only for slippage (down=second, up/flat=first)
                                            // Clone vars for spawned task (direction used for slippage allocation)
                                            let executor_clone = executor.clone();
                                            let risk_manager_clone = _risk_manager.clone();
                                            let opp_clone = opp.clone();
                                            let yes_dir_s = yes_dir.to_string();
                                            let no_dir_s = no_dir.to_string();
                                            let dashboard_trade = dashboard.clone();
                                            let trade_sym_spawn = trade_sym.clone();
                                            let runtime_order_size_cap = order_size;
                                            let runtime_execution_threshold = execution_threshold;
                                            let execution_profit_pct_spawn =
                                                decimal_to_f64(execution_profit_percentage);

                                            // Spawn async to avoid blocking orderbook updates
                                            tokio::spawn(async move {
                                                // Execute arbitrage (slippage: down=second, up/flat=first)
                                                match executor_clone
                                                    .execute_arbitrage_pair_with_max_order_size(
                                                        &opp_clone,
                                                        &yes_dir_s,
                                                        &no_dir_s,
                                                        runtime_order_size_cap,
                                                        runtime_execution_threshold,
                                                    )
                                                    .await
                                                {
                                                    Ok(result) => {
                                                        if result.success {
                                                            dashboard_trade.with_mut(|d| {
                                                                d.record_trade_success(
                                                                    &trade_sym_spawn,
                                                                    profit_usd,
                                                                    execution_profit_pct_spawn,
                                                                );
                                                            });
                                                        } else {
                                                            dashboard_trade.with_mut(|d| {
                                                                d.record_trade_failure(
                                                                    &trade_sym_spawn,
                                                                    "orders not filled",
                                                                );
                                                            });
                                                        }

                                                        // Save pair_id first, result will be moved
                                                        let pair_id = result.pair_id.clone();

                                                        // Register with risk manager (with prices for exposure calc)
                                                        risk_manager_clone.register_order_pair(
                                                            result,
                                                            opp_clone.market_id,
                                                            opp_clone.yes_token_id,
                                                            opp_clone.no_token_id,
                                                            opp_clone.yes_ask_price,
                                                            opp_clone.no_ask_price,
                                                        );

                                                        // Handle risk recovery
                                                        // Hedge strategy disabled; no action on one-sided fills
                                                        match risk_manager_clone.handle_order_pair(&pair_id).await {
                                                            Ok(action) => {
                                                                // Hedge strategy off; no MonitorForExit/SellExcess
                                                                match action {
                                                                    crate::risk::recovery::RecoveryAction::None => {
                                                                        // Normal case, no action
                                                                    }
                                                                    crate::risk::recovery::RecoveryAction::MonitorForExit { .. } => {
                                                                        info!("One-sided fill, hedge strategy disabled, no action");
                                                                    }
                                                                    crate::risk::recovery::RecoveryAction::SellExcess { .. } => {
                                                                        info!("Partial fill imbalance, hedge strategy disabled, no action");
                                                                    }
                                                                    crate::risk::recovery::RecoveryAction::ManualIntervention { reason } => {
                                                                        warn!("Manual intervention needed: {}", reason);
                                                                    }
                                                                }
                                                            }
                                                            Err(e) => {
                                                                error!("Risk handling failed: {}", e);
                                                            }
                                                        }
                                                    }
                                                    Err(e) => {
                                                        // Error details in executor; brief summary here
                                                        let error_msg = e.to_string();
                                                        dashboard_trade.with_mut(|d| {
                                                            d.record_trade_failure(&trade_sym_spawn, &error_msg);
                                                        });
                                                        // Extract simplified error
                                                        if error_msg.contains("Arbitrage failed") {
                                                            // Error already formatted
                                                            error!("{}", error_msg);
                                                        } else {
                                                            error!("Arbitrage execution failed: {}", error_msg);
                                                        }
                                                    }
                                                }
                                            });
                                        }
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            error!(error = %e, "Orderbook update error");
                            // Stream error, recreate stream
                            break;
                        }
                        None => {
                            warn!("Orderbook stream ended, recreating");
                            break;
                        }
                    }
                }

                // Position balance task
                _ = async {
                    if let Some(ref mut timer) = balance_timer {
                        timer.tick().await;
                        // Gate: only balance in the final `balance_start_secs` before
                        // window end, so GTD legs rest untouched earlier and are only
                        // cleaned up near the end. 0 = balance the whole window.
                        // The tick cadence (POSITION_BALANCE_INTERVAL_SECS, set to 1s)
                        // then drives the ~per-second re-check inside that window.
                        let remaining = (window_end - Utc::now()).num_seconds();
                        if balance_start_secs == 0 || remaining <= balance_start_secs as i64 {
                            if let Err(e) = position_balancer.check_and_balance_positions(&market_token_map).await {
                                warn!(error = %e, "Position balance check failed");
                            }
                        }
                    } else {
                        futures::future::pending::<()>().await;
                    }
                } => {
                    // Balance task done
                }

                // Periodic check: 1) new 5min window 2) wind-down trigger
                _ = sleep(Duration::from_secs(1)) => {
                    if shutdown.load(Ordering::Relaxed) {
                        return Ok(());
                    }

                    let now = Utc::now();
                    let seconds_until_end =
                        (window_end - now).num_seconds().max(0) as u32;
                    let exposure = _risk_manager.position_tracker().calculate_exposure();
                    dashboard.with_mut(|d| {
                        d.set_window(&window_label, seconds_until_end);
                        d.set_exposure(decimal_to_f64(exposure));
                    });

                    let new_window_timestamp = MarketDiscoverer::calculate_current_window_timestamp(now);

                    // If window timestamp changed, we're in a new window
                    if new_window_timestamp != current_window_timestamp {
                        info!(
                            old_window = current_window_timestamp,
                            new_window = new_window_timestamp,
                            "New 5min window detected, cancelling old subscriptions and switching"
                        );
                        // Clean slate for the new window: cancel any resting orders
                        // from the old window. GTC has no expiry, so without this its
                        // unfilled legs would leak across windows and lock collateral.
                        // Best-effort — old-window orders can't fill anyway once it
                        // closes, so log and switch regardless.
                        if let Err(e) = executor.cancel_all_orders().await {
                            warn!(error = %e, "Failed to cancel old-window orders at window switch");
                        }
                        // Drop stream to release monitor borrow, then clear old subs
                        drop(stream);
                        monitor.clear();
                        break;
                    }
                }
            }
        }

        // monitor is dropped at loop end, no manual cleanup
        info!("Current window monitoring ended, refreshing markets for next round");
    }
}
