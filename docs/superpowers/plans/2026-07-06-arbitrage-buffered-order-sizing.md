# Arbitrage Buffered Order Sizing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Improve arbitrage fill probability by requiring sufficient available shares, submitting only a configured ratio of those shares, and refusing orders whose slippage-adjusted limit prices no longer satisfy the configured execution spread.

**Architecture:** Keep opportunity detection unchanged, then add explicit pre-submit sizing and price-safety helpers in `TradingExecutor` so `main.rs` and executor-level validation use the same math. Add two env-only configuration fields for minimum available shares and order size ratio; keep price buffering on the existing `SLIPPAGE` setting.

**Tech Stack:** Rust 2021, `rust_decimal`, existing Polymarket CLOB V2 executor, `cargo test --lib`.

---

### Task 1: Add Behavior Tests First

**Files:**
- Modify: `src/trading/executor.rs`

- [ ] **Step 1: Add failing tests for size ratio and slippage threshold**

Add tests in the existing `#[cfg(test)] mod tests`:

```rust
#[test]
fn sized_order_applies_ratio_after_available_cap_and_floors_to_cents() {
    let available = TradingExecutor::capped_order_size(dec!(20), dec!(40), dec!(100));
    let scaled = TradingExecutor::apply_order_size_ratio(available, dec!(0.8));
    assert_eq!(scaled, dec!(16));

    let odd_available = TradingExecutor::capped_order_size(dec!(20.019), dec!(40), dec!(100));
    let odd_scaled = TradingExecutor::apply_order_size_ratio(odd_available, dec!(0.8));
    assert_eq!(odd_scaled, dec!(16.01));
}

#[test]
fn slippage_adjusted_prices_must_stay_under_execution_threshold() {
    let slippage = [dec!(0.01), dec!(0.01)];
    assert!(TradingExecutor::prices_with_slippage_within_threshold(
        dec!(0.45),
        dec!(0.50),
        "↑",
        "−",
        slippage,
        dec!(0.97),
    ));
    assert!(!TradingExecutor::prices_with_slippage_within_threshold(
        dec!(0.46),
        dec!(0.50),
        "↑",
        "−",
        slippage,
        dec!(0.97),
    ));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test sized_order_applies_ratio_after_available_cap_and_floors_to_cents
cargo test slippage_adjusted_prices_must_stay_under_execution_threshold
```

Expected: FAIL because the new helper methods do not exist.

### Task 2: Implement Sizing And Price Safety Helpers

**Files:**
- Modify: `src/trading/executor.rs`

- [ ] **Step 1: Add minimal helper methods**

Add helpers near the existing `capped_order_size` and slippage helpers:

```rust
fn apply_order_size_ratio(size: Decimal, ratio: Decimal) -> Decimal {
    if ratio <= dec!(0) {
        return dec!(0);
    }
    let ratio = ratio.min(dec!(1));
    (size * ratio * dec!(100)).floor() / dec!(100)
}

fn limit_price_with_slippage(price: Decimal, dir: &str, slippage: [Decimal; 2]) -> Decimal {
    let add = if dir == "↓" { slippage[1] } else { slippage[0] };
    (price + add).min(dec!(1.0))
}

fn prices_with_slippage_within_threshold(
    yes_price: Decimal,
    no_price: Decimal,
    yes_dir: &str,
    no_dir: &str,
    slippage: [Decimal; 2],
    execution_threshold: Decimal,
) -> bool {
    let yes_limit = Self::limit_price_with_slippage(yes_price, yes_dir, slippage);
    let no_limit = Self::limit_price_with_slippage(no_price, no_dir, slippage);
    yes_limit + no_limit <= execution_threshold
}
```

- [ ] **Step 2: Run tests to verify helpers pass**

Run:

```bash
cargo test sized_order_applies_ratio_after_available_cap_and_floors_to_cents
cargo test slippage_adjusted_prices_must_stay_under_execution_threshold
```

Expected: PASS.

### Task 3: Wire Config And Runtime Flow

**Files:**
- Modify: `src/config.rs`
- Modify: `src/main.rs`
- Modify: `src/trading/executor.rs`

- [ ] **Step 1: Add env config fields**

Add:

```rust
pub arbitrage_min_available_shares: f64,
pub arbitrage_order_size_ratio: f64,
```

Parse:

```rust
arbitrage_min_available_shares: env::var("ARBITRAGE_MIN_AVAILABLE_SHARES")
    .unwrap_or_else(|_| "5.0".to_string())
    .parse()
    .unwrap_or(5.0),
arbitrage_order_size_ratio: env::var("ARBITRAGE_ORDER_SIZE_RATIO")
    .unwrap_or_else(|_| "1.0".to_string())
    .parse()
    .ok()
    .filter(|v| v.is_finite() && *v > 0.0 && *v <= 1.0)
    .unwrap_or(1.0),
```

- [ ] **Step 2: Use ratio and min available before spawn**

In `main.rs`, after `max_order_size`, compute:

```rust
let min_available_shares =
    Decimal::try_from(config.arbitrage_min_available_shares).unwrap_or(dec!(5.0));
let order_size_ratio =
    Decimal::try_from(config.arbitrage_order_size_ratio).unwrap_or(dec!(1.0));
let available_size = TradingExecutor::capped_order_size(
    opp.yes_size,
    opp.no_size,
    max_order_size,
);
if available_size < min_available_shares {
    continue;
}
let order_size = TradingExecutor::apply_order_size_ratio(available_size, order_size_ratio);
```

Then compute risk costs using slippage-adjusted limit prices and pass `order_size` as the explicit executor cap.

- [ ] **Step 3: Add executor-level threshold check**

Change `execute_arbitrage_pair_with_max_order_size` to accept `execution_threshold: Decimal`, and after slippage prices are computed, return an error if:

```rust
yes_price_with_slippage + no_price_with_slippage > execution_threshold
```

Update the main call to pass the current runtime threshold.

### Task 4: Docs And Verification

**Files:**
- Modify: `.env.example`
- Modify: `README.md`
- Modify: `README.zh-CN.md`

- [ ] **Step 1: Document new env vars**

Add `ARBITRAGE_MIN_AVAILABLE_SHARES` and `ARBITRAGE_ORDER_SIZE_RATIO` beside the arbitrage order settings. Clarify that `SLIPPAGE=0.01,0.01` is the one-cent per-leg price buffer and that slippage-adjusted prices must still satisfy `yes + no <= 1 - spread`.

- [ ] **Step 2: Run verification**

Run:

```bash
cargo fmt
cargo test --lib
cargo test
```

Expected: all commands exit 0.

- [ ] **Step 3: Review diff and publish**

Run:

```bash
git diff --check
git diff --stat
git status -sb
git add docs/superpowers/plans/2026-07-06-arbitrage-buffered-order-sizing.md src/config.rs src/main.rs src/trading/executor.rs .env.example README.md README.zh-CN.md
git commit -m "feat: add buffered arbitrage order sizing"
git push -u origin codex/arbitrage-buffered-order-sizing
```
