# Embedded Control Console Design

## Goal

Add a single-container web control console to the Rust Polypulse bot so the owner can monitor and control the running process through a fixed HTTPS domain protected by an admin token.

## Architecture

The existing Rust binary remains the only process. It continues to run the trading loop, orderbook monitor, scheduled merge, wind-down, and position sync tasks. A new Axum web server runs inside the same Tokio runtime and serves both JSON APIs and an embedded HTML control page.

Web handlers do not mutate trading behavior directly. They authenticate requests, validate command payloads, and send `BotCommand` messages into a command processor. The command processor owns side effects such as pausing trading, starting manual merge, canceling orders, changing runtime config, and requesting shutdown.

## Security Model

The console is intended for remote Docker deployment behind a fixed domain with HTTPS handled by a reverse proxy. Every control and dashboard API request requires:

```text
Authorization: Bearer <ADMIN_TOKEN>
```

`ADMIN_TOKEN` is read from the runtime environment. The bot refuses to start the web server when `WEB_ENABLED=true` and `ADMIN_TOKEN` is empty.

High-risk actions require `confirm: true` in the JSON request body. High-risk actions are manual merge, cancel all orders, and shutdown. The UI also prompts before sending them, but the backend requirement is the authoritative guard.

Sensitive configuration is never exposed or editable through the API. Private keys, proxy addresses, Builder credentials, and CLOB URLs stay environment-only.

## Runtime Control

The new runtime control state contains:

- `trading_paused`
- `merge_running`
- `cancel_running`
- `shutdown_requested`
- `runtime_config`
- `last_command`
- `last_error`

The first editable runtime config fields are:

- `max_order_size_usdc`
- `arbitrage_execution_spread`
- `stop_arbitrage_before_end_minutes`
- `wind_down_before_window_end_minutes`
- `min_yes_price_threshold`
- `min_no_price_threshold`

Trading checks use this runtime config at decision time. The executor also receives the effective max-order cap at order submission time so a runtime decrease cannot be bypassed by the executor's startup config.

## API

```text
GET   /                         embedded console page
GET   /api/health               unauthenticated liveness
GET   /api/dashboard            authenticated dashboard snapshot
GET   /api/control              authenticated control snapshot
POST  /api/control/pause        pause new arbitrage execution
POST  /api/control/resume       resume arbitrage execution
POST  /api/control/merge-now    confirm=true, run one manual merge pass
POST  /api/control/cancel-all   confirm=true, cancel all open orders
POST  /api/control/shutdown     confirm=true, request graceful shutdown
PATCH /api/control/config       update runtime config whitelist
```

## Frontend

The first version is a lightweight embedded HTML/CSS/JS app. It prioritizes operator clarity over a large frontend toolchain:

- status strip for connected/paused/merge/cancel/shutdown
- hero metrics for PnL, exposure, current window, and trades
- market table for YES, NO, total, edge, and direction
- command controls for pause, resume, merge, cancel, and shutdown
- runtime config form for whitelisted fields
- recent event log

The page stores the admin token in browser local storage for the operator's browser only. API calls send the token as a Bearer token. Token leakage remains a browser/device security concern, so HTTPS and a private operator device are assumed.

## Docker

The container exposes port `8080` by default when `WEB_ENABLED=true`. The service itself can bind `0.0.0.0:8080` inside Docker, while public TLS termination should happen at the reverse proxy. Docker images do not contain `.env`, secrets, or logs.

## Tests

Automated tests cover:

- runtime config validation and patching
- control state transitions
- bearer token parsing
- confirmation enforcement for high-risk requests
- dashboard snapshot construction

Build verification must include `cargo test --locked` and `cargo build --release --locked`.
