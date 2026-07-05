# Embedded Control Console Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build an embedded, token-protected web control console for the Polypulse Rust bot.

**Architecture:** Add a `control` module for runtime state, command definitions, validation, and command responses. Add a `web` module using Axum to serve authenticated JSON APIs and an embedded HTML page. Wire both into `main.rs` so the web server and command processor run alongside the existing trading loop.

**Tech Stack:** Rust 2021, Tokio, Axum, Serde, embedded HTML/CSS/JS, Docker.

---

### Task 1: Control Domain

**Files:**
- Create: `src/control.rs`
- Modify: `src/lib.rs`

- [ ] Write failing unit tests for runtime config patch validation, state transitions, and dangerous confirmation behavior.
- [ ] Run `cargo test --locked control --lib` and confirm the tests fail because the module does not exist.
- [ ] Implement `RuntimeConfig`, `RuntimeConfigPatch`, `RuntimeControlState`, `ControlHandle`, `BotCommand`, `CommandRequest`, and `CommandResponse`.
- [ ] Run `cargo test --locked control --lib` and confirm the control tests pass.

### Task 2: Web API And Frontend

**Files:**
- Create: `src/web.rs`
- Create: `src/web/index.html`
- Modify: `src/lib.rs`
- Modify: `Cargo.toml`

- [ ] Write failing unit tests for bearer token validation, confirmation parsing, and dashboard snapshot construction.
- [ ] Run `cargo test --locked web --lib` and confirm the tests fail because the module/dependencies are missing.
- [ ] Add Axum dependency and implement the authenticated router, API handlers, embedded page, and snapshot types.
- [ ] Run `cargo test --locked web --lib` and confirm the web tests pass.

### Task 3: Main Loop Wiring

**Files:**
- Modify: `src/main.rs`
- Modify: `src/trading/executor.rs`

- [ ] Add a runtime max-order-size cap entry point to `TradingExecutor`.
- [ ] Start the web server when `WEB_ENABLED=true`.
- [ ] Start a command processor that handles pause, resume, config patching, manual merge, cancel all, and shutdown.
- [ ] Read `RuntimeConfig` and `trading_paused` in the arbitrage decision path.
- [ ] Reuse a one-pass merge helper for scheduled merge and manual merge.
- [ ] Run `cargo test --locked --lib`.

### Task 4: Docker And Docs

**Files:**
- Modify: `Dockerfile`
- Modify: `.env.example`
- Modify: `README.zh-CN.md`
- Modify: `README.md`

- [ ] Document `WEB_ENABLED`, `WEB_BIND`, and `ADMIN_TOKEN`.
- [ ] Expose port `8080` in Docker.
- [ ] Document reverse-proxy HTTPS expectations and control API risk boundaries.
- [ ] Run `cargo test --locked`.
- [ ] Run `cargo build --release --locked`.

### Task 5: Review, Commit, Push

**Files:**
- All changed files

- [ ] Review the diff for secret exposure, unsafe control endpoints, stale runtime config usage, and Docker omissions.
- [ ] Run final verification commands.
- [ ] Commit the scoped changes.
- [ ] Push the current branch to `origin`.
