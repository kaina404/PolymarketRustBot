use anyhow::Result;
use std::fs::File;
use tracing::field::{Field, Visit};
use tracing_subscriber::{
    layer::{Context, Filter, SubscriberExt},
    util::SubscriberInitExt,
    EnvFilter, Layer as _,
};

const SDK_WS_CONNECTION_TARGET: &str = "polymarket_client_sdk::ws::connection";
const SDK_WS_RESET_MARKER: &str = "ResetWithoutClosingHandshake";
const SDK_WS_CONNECTION_ERROR_PREFIX: &str = "Error handling connection";

pub fn init_logger(quiet_stdout: bool) -> Result<()> {
    // SDK 的 serde_helpers 会为 Gamma/CLOB 响应里每个未建模的新字段打 WARN
    // (如 feeType、comboStatus),属良性 API 漂移噪音,不影响反序列化。降到
    // error 级别:消除刷屏,同时保留真正的“反序列化失败”错误日志。
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"))
        .add_directive("polymarket_client_sdk::serde_helpers=error".parse()?);

    if quiet_stdout {
        let path = std::env::var("LOG_FILE").unwrap_or_else(|_| "bot.log".to_string());
        let file = File::create(&path)?;
        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(file)
                    .with_ansi(false)
                    .with_filter(SdkWsNoiseFilter),
            )
            .init();
    } else if let Ok(path) = std::env::var("LOG_FILE") {
        let file = File::create(path)?;
        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(file)
                    .with_ansi(false)
                    .with_filter(SdkWsNoiseFilter),
            )
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().with_filter(SdkWsNoiseFilter))
            .init();
    }

    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct SdkWsNoiseFilter;

impl<S> Filter<S> for SdkWsNoiseFilter {
    fn enabled(&self, _meta: &tracing::Metadata<'_>, _cx: &Context<'_, S>) -> bool {
        true
    }

    fn event_enabled(&self, event: &tracing::Event<'_>, _cx: &Context<'_, S>) -> bool {
        !is_benign_sdk_ws_reset(event)
    }
}

fn is_benign_sdk_ws_reset(event: &tracing::Event<'_>) -> bool {
    let metadata = event.metadata();
    if metadata.target() != SDK_WS_CONNECTION_TARGET || metadata.level() != &tracing::Level::ERROR {
        return false;
    }

    let mut visitor = MessageVisitor::default();
    event.record(&mut visitor);

    visitor.message.as_deref().is_some_and(|message| {
        message.contains(SDK_WS_CONNECTION_ERROR_PREFIX) && message.contains(SDK_WS_RESET_MARKER)
    })
}

#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        }
    }
}

pub fn tui_enabled_from_env() -> bool {
    if std::env::var("PLAIN_LOGS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return false;
    }
    !std::env::var("TUI_ENABLED")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::SdkWsNoiseFilter;
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::{layer::SubscriberExt, EnvFilter};

    /// serde_helpers 的良性 WARN(未知字段,如 feeType/comboStatus)应被过滤,
    /// 但同模块真正的 ERROR(反序列化失败)和其他目标的 WARN 应保留。
    #[test]
    fn serde_helpers_warn_suppressed_but_error_and_others_kept() {
        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buf);

        let env_filter = EnvFilter::new("info").add_directive(
            "polymarket_client_sdk::serde_helpers=error"
                .parse()
                .unwrap(),
        );

        let layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(move || {
                struct W(Arc<Mutex<Vec<u8>>>);
                impl std::io::Write for W {
                    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                        self.0.lock().expect("lock").extend_from_slice(b);
                        Ok(b.len())
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }
                W(Arc::clone(&sink))
            })
            .with_filter(SdkWsNoiseFilter);

        let subscriber = tracing_subscriber::registry().with(env_filter).with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: "polymarket_client_sdk::serde_helpers", "unknown field in API response");
            tracing::error!(target: "polymarket_client_sdk::serde_helpers", "deserialization failed");
            tracing::warn!(target: "polypulse::executor", "app warning stays");
        });

        let out = String::from_utf8(buf.lock().expect("lock").clone()).expect("utf8");
        assert!(
            !out.contains("unknown field in API response"),
            "serde_helpers WARN 应被过滤,实际: {out}"
        );
        assert!(
            out.contains("deserialization failed"),
            "serde_helpers ERROR 应保留,实际: {out}"
        );
        assert!(
            out.contains("app warning stays"),
            "其他目标的 WARN 应保留,实际: {out}"
        );
    }

    /// SDK 在 WebSocket 被对端直接 reset 时会自行进入重连流程，这条
    /// "Error handling connection" 不应刷成应用级 ERROR；其他错误仍需保留。
    #[test]
    fn websocket_reset_without_close_is_suppressed_but_other_errors_stay() {
        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buf);

        let env_filter = EnvFilter::new("info");

        let layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(move || {
                struct W(Arc<Mutex<Vec<u8>>>);
                impl std::io::Write for W {
                    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                        self.0.lock().expect("lock").extend_from_slice(b);
                        Ok(b.len())
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }
                W(Arc::clone(&sink))
            })
            .with_filter(SdkWsNoiseFilter);

        let subscriber = tracing_subscriber::registry().with(env_filter).with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(
                target: "polymarket_client_sdk::ws::connection",
                "Error handling connection: Error {{ kind: WebSocket, source: Some(Connection(Protocol(ResetWithoutClosingHandshake))), backtrace: <disabled> }}"
            );
            tracing::error!(
                target: "polymarket_client_sdk::ws::connection",
                "Error handling connection: authentication failed"
            );
            tracing::error!(target: "polypulse::monitor", "app error stays");
        });

        let out = String::from_utf8(buf.lock().expect("lock").clone()).expect("utf8");
        assert!(
            !out.contains("ResetWithoutClosingHandshake"),
            "SDK 自动重连 reset 噪音应被过滤,实际: {out}"
        );
        assert!(
            out.contains("authentication failed"),
            "SDK 其他连接错误应保留,实际: {out}"
        );
        assert!(
            out.contains("app error stays"),
            "应用错误应保留,实际: {out}"
        );
    }
}
