use anyhow::Result;
use std::fs::File;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

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
                    .with_ansi(false),
            )
            .init();
    } else if let Ok(path) = std::env::var("LOG_FILE") {
        let file = File::create(path)?;
        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(file)
                    .with_ansi(false),
            )
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .init();
    }

    Ok(())
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

    use tracing_subscriber::{layer::SubscriberExt, EnvFilter};

    /// serde_helpers 的良性 WARN(未知字段,如 feeType/comboStatus)应被过滤,
    /// 但同模块真正的 ERROR(反序列化失败)和其他目标的 WARN 应保留。
    #[test]
    fn serde_helpers_warn_suppressed_but_error_and_others_kept() {
        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buf);

        let env_filter = EnvFilter::new("info")
            .add_directive("polymarket_client_sdk::serde_helpers=error".parse().unwrap());

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
            });

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
}
