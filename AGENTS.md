# Repository Instructions

## Project Scope

This repository is `polypulse`, a Rust 2021 Polymarket crypto 5-minute Up/Down arbitrage bot. It connects to live markets, can place real orders, can merge/redeem positions on Polygon, and includes optional TUI and embedded web control surfaces. Treat every execution-path change as potentially money-impacting.

## Working Rules

- Keep changes narrowly scoped to the user's request.
- Do not overwrite or revert user edits. Check `git status --short` before making changes and work around unrelated dirty files.
- Never commit secrets or local runtime state. `.env`, `data/`, `bot.log`, `target/`, `_vendor/`, and IDE files are local-only.
- Prefer code paths and runtime evidence over guesses when debugging trading behavior.
- Update `.env.example` and the README when adding or changing user-facing configuration.
- Keep the project on Rust 2021; async/await and the current dependencies expect it.

## Common Commands

```bash
cargo fmt
cargo check
cargo test --lib
cargo test
cargo run
```

Use `cargo test --lib` for quick verification after focused logic changes. Use full `cargo test` before claiming broad trading, web, merge/redeem, or configuration changes are complete.

## Source Map

- `src/main.rs` starts the bot, initializes config, control state, market discovery, monitoring, trading, merge, wind-down, TUI, and optional web server.
- `src/config.rs` reads environment configuration from `.env`/process env.
- `src/control.rs` owns dynamic runtime control state, pause/resume, whitelisted runtime config, and persistence.
- `src/web.rs` exposes the embedded web dashboard and control API.
- `src/monitor/` tracks order books and arbitrage opportunities.
- `src/trading/` contains CLOB order placement, execution, reconciliation, and order models.
- `src/risk/` contains exposure, position balancing, recovery, and hedge monitoring logic.
- `src/market/` discovers and schedules current 5-minute markets.
- `src/merge.rs`, `src/redeem.rs`, `src/pusd_wrap.rs`, and relay modules handle on-chain or relayer flows.
- `src/ui/` contains the terminal dashboard.

## Configuration And Runtime State

- `.env.example` is the canonical user-facing config reference.
- Required live-trading secrets are `POLYMARKET_PRIVATE_KEY` and usually `POLYMARKET_PROXY_ADDRESS`.
- Builder credentials are required for merge/redeem flows: `POLY_BUILDER_API_KEY`, `POLY_BUILDER_SECRET`, `POLY_BUILDER_PASSPHRASE`.
- Use `SIGNATURE_TYPE=Poly1271` by default for V2 deposit wallets unless evidence shows a different wallet type.
- `CLOB_API_URL` should normally be `https://clob.polymarket.com`, not `clob-v2.polymarket.com`.
- When `WEB_ENABLED=true`, require a real `ADMIN_TOKEN` and expose the web console only behind HTTPS/reverse proxy in remote deployments.
- `CONTROL_STATE_PATH` persists pause/resume and runtime config. Persisted control state can override environment values after restart, so check the state file when config changes appear ignored.
- In Docker, mount `/app/data` if control state must survive container replacement.

## Trading Semantics To Preserve

- Arbitrage executes when `YES + NO <= 1 - ARBITRAGE_EXECUTION_SPREAD`.
- `MAX_ORDER_SIZE_USDC`, market liquidity, risk limits, and runtime control state can all cap actual order size.
- `RISK_MAX_EXPOSURE_USDC` is enforced per market window/round, not as a forever-global cap.
- Avoid one-sided exposure. Reconciliation should cancel resting remainders and unwind or repair imbalanced legs according to the current order-type path.
- `ARBITRAGE_HEDGE_GRACE_SECS` controls the grace period for lagging legs; default is 20 seconds.
- FOK/FAK fills are terminal; GTC/GTD orders can rest in the book and may need grace-period handling.
- Wind-down should cancel open orders, merge balanced YES/NO where possible, then handle remaining single-leg exposure according to configured limits.
- Current one-sided positions may be non-mergeable; do not assume merge/redeem can fix them.

## Merge, Redeem, And Gas Notes

- Merge/redeem uses Polygon and may hit RPC throttling. Prefer a private or paid `RPC_URL` for batch wind-down activity.
- For Safe/deposit-wallet execution, signer EOA needs enough POL for gas.
- Keep Safe gas-limit and fee-cap logic conservative; failures such as `execution reverted, data: 0x` can be caused by insufficient POL for the outer Safe transaction gas budget.
- Do not remove gas overhead calculations or fee caps without a replacement verified against Safe execution.

## Testing Guidance

- Add or update focused unit tests for trading math, risk limits, control-state persistence, gas calculations, and reconciliation behavior.
- For config changes, test parsing defaults and env overrides.
- For web/control changes, test command processing and persistence where practical.
- For live API behavior, prefer dry inspection, mocked tests, or explicit user approval before running code that can trade.

## Documentation Expectations

- Keep README and README.zh-CN aligned for user-facing behavior when practical.
- Document new environment variables in `.env.example`.
- Call out restart requirements and persistence precedence for runtime configuration changes.


## 参考文档

| 主题 | 链接 |
|------|------|
| Polymarket API 总览 | https://docs.polymarket.com/api-reference/introduction |
| Gamma API | https://docs.polymarket.com/developers/gamma-markets-api/overview |
| CLOB API | https://docs.polymarket.com/developers/CLOB/quickstart |
| WebSocket API | https://docs.polymarket.com/market-data/websocket/overview |
| CTF | https://docs.polymarket.com/trading/ctf/overview |
| NegRisk | https://docs.polymarket.com/advanced/neg-risk |
| py-clob-client v1 | https://github.com/Polymarket/py-clob-client |
| py-clob-client v2 | https://github.com/Polymarket/py-clob-client-v2 |
| Alchemy 文档 | https://www.alchemy.com/docs/get-started |
| Binance Spot WebSocket Streams | https://github.com/binance/binance-spot-api-docs/blob/master/web-socket-streams.md |