use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct RuntimeConfig {
    pub max_order_size_usdc: f64,
    pub arbitrage_execution_spread: f64,
    pub stop_arbitrage_before_end_seconds: u64,
    pub wind_down_before_window_end_seconds: u64,
    pub min_yes_price_threshold: f64,
    pub min_no_price_threshold: f64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_order_size_usdc: 100.0,
            arbitrage_execution_spread: 0.01,
            stop_arbitrage_before_end_seconds: 0,
            wind_down_before_window_end_seconds: 0,
            min_yes_price_threshold: 0.0,
            min_no_price_threshold: 0.0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RuntimeConfigPatch {
    pub max_order_size_usdc: Option<f64>,
    pub arbitrage_execution_spread: Option<f64>,
    pub stop_arbitrage_before_end_seconds: Option<u64>,
    pub wind_down_before_window_end_seconds: Option<u64>,
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
        if let Some(value) = self.stop_arbitrage_before_end_seconds {
            next.stop_arbitrage_before_end_seconds = value;
        }
        if let Some(value) = self.wind_down_before_window_end_seconds {
            next.wind_down_before_window_end_seconds = value;
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
            trading_paused: true,
            merge_running: false,
            cancel_running: false,
            shutdown_requested: false,
            runtime_config,
            last_command: None,
            last_error: None,
            updated_at: Utc::now(),
        }
    }

    fn from_persisted(persisted: PersistedControlState) -> Self {
        Self {
            trading_paused: true,
            merge_running: false,
            cancel_running: false,
            shutdown_requested: false,
            runtime_config: persisted.runtime_config,
            last_command: None,
            last_error: None,
            updated_at: persisted.updated_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct PersistedControlState {
    version: u8,
    trading_paused: bool,
    runtime_config: RuntimeConfig,
    updated_at: DateTime<Utc>,
}

impl PersistedControlState {
    const VERSION: u8 = 1;

    fn from_runtime(state: &RuntimeControlState) -> Self {
        Self {
            version: Self::VERSION,
            trading_paused: state.trading_paused,
            runtime_config: state.runtime_config,
            updated_at: state.updated_at,
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
    persistence_path: Option<Arc<PathBuf>>,
}

impl ControlHandle {
    pub fn new(runtime_config: RuntimeConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(RuntimeControlState::new(runtime_config))),
            persistence_path: None,
        }
    }

    pub fn with_persistence(
        runtime_config: RuntimeConfig,
        path: impl Into<PathBuf>,
    ) -> Result<Self, String> {
        let path = path.into();
        let state = load_persisted_control_state(&path)?
            .map(RuntimeControlState::from_persisted)
            .unwrap_or_else(|| RuntimeControlState::new(runtime_config));

        Ok(Self {
            inner: Arc::new(RwLock::new(state)),
            persistence_path: Some(Arc::new(path)),
        })
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

    pub fn set_trading_paused(
        &self,
        paused: bool,
        command: impl Into<String>,
    ) -> Result<(), String> {
        let command = command.into();
        self.update_persistent(|state| {
            state.trading_paused = paused;
            state.last_command = Some(command);
            state.last_error = None;
            Ok(())
        })
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
        let command = command.into();
        self.update_persistent(|state| {
            let mut next = state.runtime_config;
            patch.apply_to(&mut next)?;
            state.runtime_config = next;
            state.last_command = Some(command);
            state.last_error = None;
            Ok(next)
        })
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

    fn update_persistent<R>(
        &self,
        f: impl FnOnce(&mut RuntimeControlState) -> Result<R, String>,
    ) -> Result<R, String> {
        let mut guard = self.inner.write().expect("control state lock");
        let original = guard.clone();

        match f(&mut guard) {
            Ok(value) => {
                guard.updated_at = Utc::now();
                if let Err(err) = self.persist_locked(&guard) {
                    *guard = original;
                    return Err(err);
                }
                Ok(value)
            }
            Err(err) => {
                guard.last_error = Some(err.clone());
                guard.updated_at = Utc::now();
                Err(err)
            }
        }
    }

    fn persist_locked(&self, state: &RuntimeControlState) -> Result<(), String> {
        let Some(path) = &self.persistence_path else {
            return Ok(());
        };
        save_persisted_control_state(path, &PersistedControlState::from_runtime(state))
    }
}

fn load_persisted_control_state(path: &Path) -> Result<Option<PersistedControlState>, String> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(format!(
                "failed to read control state {}: {err}",
                path.display()
            ));
        }
    };

    let state: PersistedControlState = serde_json::from_str(&contents)
        .map_err(|err| format!("failed to parse control state {}: {err}", path.display()))?;
    if state.version != PersistedControlState::VERSION {
        return Err(format!(
            "unsupported control state version {} in {}",
            state.version,
            path.display()
        ));
    }
    Ok(Some(state))
}

fn save_persisted_control_state(path: &Path, state: &PersistedControlState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "failed to create control state directory {}: {err}",
                    parent.display()
                )
            })?;
        }
    }

    let bytes = serde_json::to_vec_pretty(state).map_err(|err| {
        format!(
            "failed to serialize control state {}: {err}",
            path.display()
        )
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("control_state.json");
    let tmp_path = path.with_file_name(format!(".{file_name}.tmp-{}", uuid::Uuid::new_v4()));

    fs::write(&tmp_path, bytes).map_err(|err| {
        format!(
            "failed to write control state {}: {err}",
            tmp_path.display()
        )
    })?;
    fs::rename(&tmp_path, path).map_err(|err| {
        let _ = fs::remove_file(&tmp_path);
        format!("failed to replace control state {}: {err}", path.display())
    })
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
    use std::fs;
    use std::path::PathBuf;

    fn temp_control_state_path() -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "polypulse-control-state-{}.json",
            uuid::Uuid::new_v4()
        ));
        path
    }

    #[test]
    fn runtime_config_patch_updates_only_supplied_fields() {
        let mut config = RuntimeConfig {
            max_order_size_usdc: 100.0,
            arbitrage_execution_spread: 0.01,
            stop_arbitrage_before_end_seconds: 0,
            wind_down_before_window_end_seconds: 0,
            min_yes_price_threshold: 0.0,
            min_no_price_threshold: 0.0,
        };

        RuntimeConfigPatch {
            max_order_size_usdc: Some(25.5),
            arbitrage_execution_spread: None,
            stop_arbitrage_before_end_seconds: Some(1),
            wind_down_before_window_end_seconds: None,
            min_yes_price_threshold: None,
            min_no_price_threshold: Some(0.12),
        }
        .apply_to(&mut config)
        .expect("valid patch");

        assert_eq!(config.max_order_size_usdc, 25.5);
        assert_eq!(config.arbitrage_execution_spread, 0.01);
        assert_eq!(config.stop_arbitrage_before_end_seconds, 1);
        assert_eq!(config.wind_down_before_window_end_seconds, 0);
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

        handle
            .set_trading_paused(true, "operator paused")
            .expect("pause");
        let snapshot = handle.snapshot();
        assert!(snapshot.trading_paused);
        assert_eq!(snapshot.last_command.as_deref(), Some("operator paused"));

        handle
            .set_trading_paused(false, "operator resumed")
            .expect("resume");
        let snapshot = handle.snapshot();
        assert!(!snapshot.trading_paused);
        assert_eq!(snapshot.last_command.as_deref(), Some("operator resumed"));
    }

    #[test]
    fn control_handle_forces_trading_paused_after_restart() {
        let path = temp_control_state_path();
        let mut restarted_config = RuntimeConfig::default();
        restarted_config.max_order_size_usdc = 250.0;

        {
            let handle = ControlHandle::with_persistence(RuntimeConfig::default(), path.clone())
                .expect("create persistent control handle");
            handle
                .set_trading_paused(false, "operator resumed")
                .expect("persist active trading");
            handle
                .update_runtime_config(
                    &RuntimeConfigPatch {
                        max_order_size_usdc: Some(42.0),
                        arbitrage_execution_spread: Some(0.03),
                        ..RuntimeConfigPatch::default()
                    },
                    "operator updated config",
                )
                .expect("persist runtime config");
            handle.set_merge_running(true);
            handle.set_cancel_running(true);
        }

        let restored = ControlHandle::with_persistence(restarted_config, path.clone())
            .expect("restore persistent control handle");
        let snapshot = restored.snapshot();
        assert!(snapshot.trading_paused);
        assert_eq!(snapshot.runtime_config.max_order_size_usdc, 42.0);
        assert_eq!(snapshot.runtime_config.arbitrage_execution_spread, 0.03);
        assert!(!snapshot.merge_running);
        assert!(!snapshot.cancel_running);

        let _ = fs::remove_file(path);
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
