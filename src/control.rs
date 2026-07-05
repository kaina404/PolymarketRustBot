use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct RuntimeConfig {
    pub max_order_size_usdc: f64,
    pub arbitrage_execution_spread: f64,
    pub stop_arbitrage_before_end_minutes: u64,
    pub wind_down_before_window_end_minutes: u64,
    pub min_yes_price_threshold: f64,
    pub min_no_price_threshold: f64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_order_size_usdc: 100.0,
            arbitrage_execution_spread: 0.01,
            stop_arbitrage_before_end_minutes: 0,
            wind_down_before_window_end_minutes: 0,
            min_yes_price_threshold: 0.0,
            min_no_price_threshold: 0.0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RuntimeConfigPatch {
    pub max_order_size_usdc: Option<f64>,
    pub arbitrage_execution_spread: Option<f64>,
    pub stop_arbitrage_before_end_minutes: Option<u64>,
    pub wind_down_before_window_end_minutes: Option<u64>,
    pub min_yes_price_threshold: Option<f64>,
    pub min_no_price_threshold: Option<f64>,
}

impl RuntimeConfigPatch {
    pub fn apply_to(&self, config: &mut RuntimeConfig) -> Result<(), String> {
        let mut next = *config;

        if let Some(value) = self.max_order_size_usdc {
            validate_positive_finite("max_order_size_usdc", value)?;
            next.max_order_size_usdc = value;
        }
        if let Some(value) = self.arbitrage_execution_spread {
            validate_unit_interval_open_upper("arbitrage_execution_spread", value)?;
            next.arbitrage_execution_spread = value;
        }
        if let Some(value) = self.stop_arbitrage_before_end_minutes {
            next.stop_arbitrage_before_end_minutes = value;
        }
        if let Some(value) = self.wind_down_before_window_end_minutes {
            next.wind_down_before_window_end_minutes = value;
        }
        if let Some(value) = self.min_yes_price_threshold {
            validate_unit_interval_closed("min_yes_price_threshold", value)?;
            next.min_yes_price_threshold = value;
        }
        if let Some(value) = self.min_no_price_threshold {
            validate_unit_interval_closed("min_no_price_threshold", value)?;
            next.min_no_price_threshold = value;
        }

        *config = next;
        Ok(())
    }
}

fn validate_positive_finite(name: &str, value: f64) -> Result<(), String> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(format!("{name} must be a finite number greater than 0"))
    }
}

fn validate_unit_interval_open_upper(name: &str, value: f64) -> Result<(), String> {
    if value.is_finite() && (0.0..1.0).contains(&value) {
        Ok(())
    } else {
        Err(format!("{name} must be a finite number >= 0 and < 1"))
    }
}

fn validate_unit_interval_closed(name: &str, value: f64) -> Result<(), String> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(format!("{name} must be a finite number between 0 and 1"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeControlState {
    pub trading_paused: bool,
    pub merge_running: bool,
    pub cancel_running: bool,
    pub shutdown_requested: bool,
    pub runtime_config: RuntimeConfig,
    pub last_command: Option<String>,
    pub last_error: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl RuntimeControlState {
    pub fn new(runtime_config: RuntimeConfig) -> Self {
        Self {
            trading_paused: false,
            merge_running: false,
            cancel_running: false,
            shutdown_requested: false,
            runtime_config,
            last_command: None,
            last_error: None,
            updated_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlSnapshot {
    pub trading_paused: bool,
    pub merge_running: bool,
    pub cancel_running: bool,
    pub shutdown_requested: bool,
    pub runtime_config: RuntimeConfig,
    pub last_command: Option<String>,
    pub last_error: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl From<&RuntimeControlState> for ControlSnapshot {
    fn from(state: &RuntimeControlState) -> Self {
        Self {
            trading_paused: state.trading_paused,
            merge_running: state.merge_running,
            cancel_running: state.cancel_running,
            shutdown_requested: state.shutdown_requested,
            runtime_config: state.runtime_config,
            last_command: state.last_command.clone(),
            last_error: state.last_error.clone(),
            updated_at: state.updated_at,
        }
    }
}

#[derive(Clone)]
pub struct ControlHandle {
    inner: Arc<RwLock<RuntimeControlState>>,
}

impl ControlHandle {
    pub fn new(runtime_config: RuntimeConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(RuntimeControlState::new(runtime_config))),
        }
    }

    pub fn snapshot(&self) -> ControlSnapshot {
        let guard = self.inner.read().expect("control state lock");
        ControlSnapshot::from(&*guard)
    }

    pub fn runtime_config(&self) -> RuntimeConfig {
        self.inner
            .read()
            .expect("control state lock")
            .runtime_config
    }

    pub fn trading_paused(&self) -> bool {
        self.inner
            .read()
            .expect("control state lock")
            .trading_paused
    }

    pub fn set_trading_paused(&self, paused: bool, command: impl Into<String>) {
        self.update(|state| {
            state.trading_paused = paused;
            state.last_command = Some(command.into());
            state.last_error = None;
        });
    }

    pub fn set_merge_running(&self, running: bool) {
        self.update(|state| {
            state.merge_running = running;
        });
    }

    pub fn set_cancel_running(&self, running: bool) {
        self.update(|state| {
            state.cancel_running = running;
        });
    }

    pub fn request_shutdown(&self, command: impl Into<String>) {
        self.update(|state| {
            state.shutdown_requested = true;
            state.last_command = Some(command.into());
            state.last_error = None;
        });
    }

    pub fn update_runtime_config(
        &self,
        patch: &RuntimeConfigPatch,
        command: impl Into<String>,
    ) -> Result<RuntimeConfig, String> {
        let mut guard = self.inner.write().expect("control state lock");
        let mut next = guard.runtime_config;
        patch.apply_to(&mut next).map_err(|err| {
            guard.last_error = Some(err.clone());
            guard.updated_at = Utc::now();
            err
        })?;
        guard.runtime_config = next;
        guard.last_command = Some(command.into());
        guard.last_error = None;
        guard.updated_at = Utc::now();
        Ok(next)
    }

    pub fn record_command(&self, command: impl Into<String>) {
        self.update(|state| {
            state.last_command = Some(command.into());
            state.last_error = None;
        });
    }

    pub fn record_error(&self, error: impl Into<String>) {
        self.update(|state| {
            state.last_error = Some(error.into());
        });
    }

    fn update(&self, f: impl FnOnce(&mut RuntimeControlState)) {
        let mut guard = self.inner.write().expect("control state lock");
        f(&mut guard);
        guard.updated_at = Utc::now();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BotCommand {
    PauseTrading,
    ResumeTrading,
    RunMergeNow { confirm: bool },
    CancelAllOrders { confirm: bool },
    UpdateRuntimeConfig { patch: RuntimeConfigPatch },
    Shutdown { confirm: bool },
}

impl BotCommand {
    pub fn name(&self) -> &'static str {
        match self {
            BotCommand::PauseTrading => "pause_trading",
            BotCommand::ResumeTrading => "resume_trading",
            BotCommand::RunMergeNow { .. } => "run_merge_now",
            BotCommand::CancelAllOrders { .. } => "cancel_all_orders",
            BotCommand::UpdateRuntimeConfig { .. } => "update_runtime_config",
            BotCommand::Shutdown { .. } => "shutdown",
        }
    }

    pub fn is_confirmed(&self) -> bool {
        match self {
            BotCommand::RunMergeNow { confirm }
            | BotCommand::CancelAllOrders { confirm }
            | BotCommand::Shutdown { confirm } => *confirm,
            BotCommand::PauseTrading
            | BotCommand::ResumeTrading
            | BotCommand::UpdateRuntimeConfig { .. } => true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandResponse {
    pub status: CommandStatus,
    pub message: String,
}

impl CommandResponse {
    pub fn accepted(message: impl Into<String>) -> Self {
        Self {
            status: CommandStatus::Accepted,
            message: message.into(),
        }
    }

    pub fn rejected(message: impl Into<String>) -> Self {
        Self {
            status: CommandStatus::Rejected,
            message: message.into(),
        }
    }
}

pub struct CommandRequest {
    pub command: BotCommand,
    pub respond_to: oneshot::Sender<CommandResponse>,
}

impl CommandRequest {
    pub fn new(command: BotCommand, respond_to: oneshot::Sender<CommandResponse>) -> Self {
        Self {
            command,
            respond_to,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_config_patch_updates_only_supplied_fields() {
        let mut config = RuntimeConfig {
            max_order_size_usdc: 100.0,
            arbitrage_execution_spread: 0.01,
            stop_arbitrage_before_end_minutes: 0,
            wind_down_before_window_end_minutes: 0,
            min_yes_price_threshold: 0.0,
            min_no_price_threshold: 0.0,
        };

        RuntimeConfigPatch {
            max_order_size_usdc: Some(25.5),
            arbitrage_execution_spread: None,
            stop_arbitrage_before_end_minutes: Some(1),
            wind_down_before_window_end_minutes: None,
            min_yes_price_threshold: None,
            min_no_price_threshold: Some(0.12),
        }
        .apply_to(&mut config)
        .expect("valid patch");

        assert_eq!(config.max_order_size_usdc, 25.5);
        assert_eq!(config.arbitrage_execution_spread, 0.01);
        assert_eq!(config.stop_arbitrage_before_end_minutes, 1);
        assert_eq!(config.wind_down_before_window_end_minutes, 0);
        assert_eq!(config.min_yes_price_threshold, 0.0);
        assert_eq!(config.min_no_price_threshold, 0.12);
    }

    #[test]
    fn runtime_config_patch_rejects_unsafe_values() {
        let mut config = RuntimeConfig::default();

        let err = RuntimeConfigPatch {
            max_order_size_usdc: Some(0.0),
            ..RuntimeConfigPatch::default()
        }
        .apply_to(&mut config)
        .expect_err("zero max order size must be rejected");
        assert!(err.contains("max_order_size_usdc"));

        let err = RuntimeConfigPatch {
            arbitrage_execution_spread: Some(1.0),
            ..RuntimeConfigPatch::default()
        }
        .apply_to(&mut config)
        .expect_err("spread >= 1 must be rejected");
        assert!(err.contains("arbitrage_execution_spread"));

        let err = RuntimeConfigPatch {
            min_yes_price_threshold: Some(1.2),
            ..RuntimeConfigPatch::default()
        }
        .apply_to(&mut config)
        .expect_err("price threshold > 1 must be rejected");
        assert!(err.contains("min_yes_price_threshold"));
    }

    #[test]
    fn control_handle_tracks_pause_and_last_command() {
        let handle = ControlHandle::new(RuntimeConfig::default());

        handle.set_trading_paused(true, "operator paused");
        let snapshot = handle.snapshot();
        assert!(snapshot.trading_paused);
        assert_eq!(snapshot.last_command.as_deref(), Some("operator paused"));

        handle.set_trading_paused(false, "operator resumed");
        let snapshot = handle.snapshot();
        assert!(!snapshot.trading_paused);
        assert_eq!(snapshot.last_command.as_deref(), Some("operator resumed"));
    }

    #[test]
    fn dangerous_commands_require_confirmation() {
        assert!(!BotCommand::RunMergeNow { confirm: false }.is_confirmed());
        assert!(BotCommand::RunMergeNow { confirm: true }.is_confirmed());
        assert!(!BotCommand::CancelAllOrders { confirm: false }.is_confirmed());
        assert!(BotCommand::CancelAllOrders { confirm: true }.is_confirmed());
        assert!(!BotCommand::Shutdown { confirm: false }.is_confirmed());
        assert!(BotCommand::Shutdown { confirm: true }.is_confirmed());
        assert!(BotCommand::PauseTrading.is_confirmed());
    }
}
