use alloy::signers::local::LocalSigner;
use alloy::signers::Signer;
use anyhow::Result;
use chrono::Utc;
use polymarket_client_sdk_v2::clob::types::request::OrderBookSummaryRequest;
use polymarket_client_sdk_v2::clob::types::response::{CancelOrdersResponse, PostOrderResponse};
use polymarket_client_sdk_v2::clob::types::{OrderStatusType, OrderType, Side};
use polymarket_client_sdk_v2::error::{Error as SdkError, Method, Status, StatusCode};
use polymarket_client_sdk_v2::types::{Decimal, U256};
use polymarket_client_sdk_v2::POLYGON;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use std::str::FromStr;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::monitor::arbitrage::ArbitrageOpportunity;
use crate::trading::AuthenticatedClobClient;

pub struct OrderPairResult {
    pub pair_id: String,
    pub yes_order_id: String,
    pub no_order_id: String,
    pub yes_filled: Decimal,
    pub no_filled: Decimal,
    pub yes_size: Decimal,
    pub no_size: Decimal,
    pub success: bool,
}

pub struct TradingExecutor {
    client: AuthenticatedClobClient,
    private_key: String,
    max_order_size: Decimal,
    slippage: [Decimal; 2], // [first, second]: down uses second, up/flat uses first
    gtd_expiration_secs: u64,
    arbitrage_order_type: OrderType,
    hedge_grace_secs: u64,
}

impl TradingExecutor {
    pub fn from_client(
        client: AuthenticatedClobClient,
        private_key: String,
        max_order_size_usdc: f64,
        slippage: [f64; 2],
        gtd_expiration_secs: u64,
        arbitrage_order_type: OrderType,
        hedge_grace_secs: u64,
    ) -> Self {
        Self {
            client,
            private_key,
            max_order_size: Decimal::try_from(max_order_size_usdc)
                .unwrap_or(rust_decimal_macros::dec!(100.0)),
            slippage: [
                Decimal::try_from(slippage[0]).unwrap_or(dec!(0.0)),
                Decimal::try_from(slippage[1]).unwrap_or(dec!(0.01)),
            ],
            gtd_expiration_secs,
            arbitrage_order_type,
            hedge_grace_secs,
        }
    }

    pub fn client(&self) -> &AuthenticatedClobClient {
        &self.client
    }

    /// Verify auth actually succeeded via api_keys()
    pub async fn verify_authentication(&self) -> Result<()> {
        self.client
            .api_keys()
            .await
            .map_err(|e| anyhow::anyhow!("Auth verification failed: API error: {}", e))?;
        Ok(())
    }

    /// Cancel all orders for this account (for wind-down)
    pub async fn cancel_all_orders(&self) -> Result<CancelOrdersResponse> {
        self.client
            .cancel_all_orders()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to cancel all orders: {}", e))
    }

    /// Place GTC sell at given price (wind-down: market-intent for one-sided leg)
    pub async fn sell_at_price(
        &self,
        token_id: U256,
        price: Decimal,
        size: Decimal,
    ) -> Result<PostOrderResponse> {
        let signer = LocalSigner::from_str(&self.private_key)?.with_chain_id(Some(POLYGON));
        let order = self
            .client
            .limit_order()
            .token_id(token_id)
            .side(Side::Sell)
            .price(price)
            .size(size)
            .order_type(OrderType::GTC)
            .build()
            .await?;
        let signed = self.client.sign(&signer, order).await?;
        self.client
            .post_order(signed)
            .await
            .map_err(|e| anyhow::anyhow!("Sell order submit failed: {}", e))
    }

    /// Slippage by direction: down(↓) uses second, up(↑) and flat(−/empty) use first
    fn slippage_for_direction(&self, dir: &str) -> Decimal {
        if dir == "↓" {
            self.slippage[1]
        } else {
            self.slippage[0]
        }
    }

    fn capped_order_size(yes_size: Decimal, no_size: Decimal, max_order_size: Decimal) -> Decimal {
        yes_size.min(no_size).min(max_order_size)
    }

    /// Whether to send the second (hedging) leg, given the first leg's immediate
    /// fill amount. Only send the second leg once the first has actually filled —
    /// otherwise sending it risks a one-sided (naked) position when the first leg
    /// rests unfilled and the second fills alone.
    fn should_send_second_leg(first_taking_amount: Decimal) -> bool {
        first_taking_amount > dec!(0)
    }

    /// All arbitrage order types submit both legs concurrently. FOK/FAK are
    /// terminal IOC; GTD/GTC are left resting with no auto-cancel (GTD clears via
    /// its expiry, GTC via cancel_all at window switch). None stay gated.
    fn is_concurrent_order_type(order_type: &OrderType) -> bool {
        matches!(
            order_type,
            OrderType::FOK | OrderType::FAK | OrderType::GTD | OrderType::GTC
        )
    }

    fn is_terminal_ioc_order_type(order_type: &OrderType) -> bool {
        matches!(order_type, OrderType::FOK | OrderType::FAK)
    }

    fn ioc_no_match_zero_fill_response(e: &SdkError) -> Option<PostOrderResponse> {
        let status = e.downcast_ref::<Status>()?;
        if status.status_code != StatusCode::BAD_REQUEST
            || status.method != Method::POST
            || status.path != "/order"
        {
            return None;
        }

        let body = serde_json::from_str::<serde_json::Value>(&status.message).ok();
        let error_msg = body
            .as_ref()
            .and_then(|value| value.get("error"))
            .and_then(|value| value.as_str())
            .unwrap_or(status.message.as_str());
        let lower = error_msg.to_ascii_lowercase();
        if !lower.contains("no orders found to match")
            || !(lower.contains("fak") || lower.contains("fok"))
        {
            return None;
        }
        let order_id = body
            .as_ref()
            .and_then(|value| value.get("orderID").or_else(|| value.get("order_id")))
            .and_then(|value| value.as_str())
            .unwrap_or_default();

        Some(
            PostOrderResponse::builder()
                .error_msg(error_msg.to_string())
                .making_amount(dec!(0))
                .taking_amount(dec!(0))
                .order_id(order_id.to_string())
                .status(OrderStatusType::Unmatched)
                .success(false)
                .build(),
        )
    }

    fn gcd_u128(mut a: u128, mut b: u128) -> u128 {
        while b != 0 {
            let r = a % b;
            a = b;
            b = r;
        }
        a
    }

    fn lcm_u128(a: u128, b: u128) -> u128 {
        if a == 0 || b == 0 {
            return 0;
        }
        (a / Self::gcd_u128(a, b)).saturating_mul(b)
    }

    fn decimal_floor_units(value: Decimal, scale: u32) -> u128 {
        if value <= dec!(0) {
            return 0;
        }
        let factor = Decimal::from(10_u128.pow(scale));
        (value * factor).floor().to_u128().unwrap_or(u128::MAX)
    }

    fn decimal_from_units(units: u128, scale: u32) -> Decimal {
        Decimal::from_i128_with_scale(i128::try_from(units).unwrap_or(i128::MAX), scale)
    }

    fn buy_size_unit_multiple_for_cent_notional(price: Decimal) -> u128 {
        if price <= dec!(0) {
            return u128::MAX;
        }

        let price = price.normalize();
        let price_units = match u128::try_from(price.mantissa()) {
            Ok(units) if units > 0 => units,
            _ => return u128::MAX,
        };
        let price_scale = price.scale();
        let price_denominator = 10_u128.pow(price_scale);
        price_denominator / Self::gcd_u128(price_units, price_denominator)
    }

    fn buy_size_for_api_amount_precision(price: Decimal, size: Decimal) -> Decimal {
        let size_units = Self::decimal_floor_units(size, 2);
        let multiple = Self::buy_size_unit_multiple_for_cent_notional(price);
        if size_units == 0 || multiple == 0 || multiple == u128::MAX {
            return dec!(0);
        }

        Self::decimal_from_units(size_units - (size_units % multiple), 2)
    }

    fn buy_pair_size_for_api_amount_precision(
        yes_price: Decimal,
        no_price: Decimal,
        size: Decimal,
    ) -> Decimal {
        let size_units = Self::decimal_floor_units(size, 2);
        let yes_multiple = Self::buy_size_unit_multiple_for_cent_notional(yes_price);
        let no_multiple = Self::buy_size_unit_multiple_for_cent_notional(no_price);
        let multiple = Self::lcm_u128(yes_multiple, no_multiple);
        if size_units == 0 || multiple == 0 || multiple == u128::MAX {
            return dec!(0);
        }

        Self::decimal_from_units(size_units - (size_units % multiple), 2)
    }

    /// Execute arbitrage: submit YES+NO via V2; concurrency depends on order type.
    /// yes_dir / no_dir: direction "↑" "↓" "−" or "" for slippage (down=second, up/flat=first)
    pub async fn execute_arbitrage_pair(
        &self,
        opp: &ArbitrageOpportunity,
        yes_dir: &str,
        no_dir: &str,
    ) -> Result<OrderPairResult> {
        self.execute_arbitrage_pair_with_max_order_size(opp, yes_dir, no_dir, self.max_order_size)
            .await
    }

    /// Execute arbitrage with an explicit runtime max-order cap.
    pub async fn execute_arbitrage_pair_with_max_order_size(
        &self,
        opp: &ArbitrageOpportunity,
        yes_dir: &str,
        no_dir: &str,
        max_order_size: Decimal,
    ) -> Result<OrderPairResult> {
        let total_start = Instant::now();

        // FOK/FAK and GTD fire both legs concurrently. FOK/FAK are terminal IOC
        // orders; GTD legs are left resting (no auto-cancel) until they fill or
        // the GTD expiry elapses. GTC stays gated & sequential because it can
        // rest forever.
        let concurrent = Self::is_concurrent_order_type(&self.arbitrage_order_type);

        let expiry_info = if matches!(self.arbitrage_order_type, OrderType::GTD) {
            format!("expiry:{}s", self.gtd_expiration_secs)
        } else {
            "no expiry".to_string()
        };
        debug!(
            market_id = %opp.market_id,
            profit_pct = %opp.profit_percentage,
            order_type = %self.arbitrage_order_type,
            "Execute arbitrage (V2 {}, type:{}, {})",
            if concurrent { "concurrent" } else { "sequential" },
            self.arbitrage_order_type,
            expiry_info
        );

        let yes_token_id = U256::from_str(&opp.yes_token_id.to_string())?;
        let no_token_id = U256::from_str(&opp.no_token_id.to_string())?;

        let order_size = Self::capped_order_size(opp.yes_size, opp.no_size, max_order_size);

        // Guard: if either leg's size < 5 (Polymarket min order size), skip the whole pair
        // to avoid one leg filling and leaving a one-sided exposure. Both legs share
        // order_size, so a single return means neither Up nor Down is placed.
        if order_size < dec!(5) {
            warn!(
                "⏭️ Skip arbitrage pair | size:{} < min 5 shares | market:{}",
                order_size, opp.market_id
            );
            return Err(anyhow::anyhow!(
                "order size {} below minimum 5 shares",
                order_size
            ));
        }

        let pair_id = Uuid::new_v4().to_string();
        let expiration = Utc::now() + chrono::Duration::seconds(self.gtd_expiration_secs as i64);

        let yes_slippage_apply = self.slippage_for_direction(yes_dir);
        let no_slippage_apply = self.slippage_for_direction(no_dir);
        let yes_price_with_slippage = (opp.yes_ask_price + yes_slippage_apply).min(dec!(1.0));
        let no_price_with_slippage = (opp.no_ask_price + no_slippage_apply).min(dec!(1.0));
        let raw_order_size = order_size;
        let order_size = Self::buy_pair_size_for_api_amount_precision(
            yes_price_with_slippage,
            no_price_with_slippage,
            raw_order_size,
        );
        if order_size < dec!(5) {
            warn!(
                "⏭️ Skip arbitrage pair | size:{} -> {} after API amount precision | market:{}",
                raw_order_size, order_size, opp.market_id
            );
            return Err(anyhow::anyhow!(
                "order size {} below minimum 5 shares after API amount precision adjustment",
                order_size
            ));
        }
        if order_size < raw_order_size {
            debug!(
                "Buy size adjusted for API amount precision | market:{} | {} -> {} | YES:{} NO:{}",
                opp.market_id,
                raw_order_size,
                order_size,
                yes_price_with_slippage,
                no_price_with_slippage
            );
        }

        info!(
            "📋 Level | YES {:.4}×{:.2} NO {:.4}×{:.2}",
            yes_price_with_slippage, order_size, no_price_with_slippage, order_size
        );

        let expiry_suffix = if matches!(self.arbitrage_order_type, OrderType::GTD) {
            format!(" | GTD {}s", self.gtd_expiration_secs)
        } else {
            String::new()
        };
        info!(
            "📤 Order | YES {:.4}→{:.4}×{} NO {:.4}→{:.4}×{} | {}{}",
            opp.yes_ask_price,
            yes_price_with_slippage,
            order_size,
            opp.no_ask_price,
            no_price_with_slippage,
            order_size,
            self.arbitrage_order_type,
            expiry_suffix
        );

        let yes_amount_usd = yes_price_with_slippage * order_size;
        let no_amount_usd = no_price_with_slippage * order_size;
        if yes_amount_usd <= dec!(1) || no_amount_usd <= dec!(1) {
            warn!(
                "⏭️ Skip order | YES:{:.2} pUSD NO:{:.2} pUSD | both must be > $1",
                yes_amount_usd, no_amount_usd
            );
            return Err(anyhow::anyhow!(
                "Order size below min: YES {:.2} pUSD, NO {:.2} pUSD; both must be > $1",
                yes_amount_usd,
                no_amount_usd
            ));
        }

        let build_start = Instant::now();
        let (yes_order, no_order) = tokio::join!(
            async {
                let b = self
                    .client
                    .limit_order()
                    .token_id(yes_token_id)
                    .side(Side::Buy)
                    .price(yes_price_with_slippage)
                    .size(order_size)
                    .order_type(self.arbitrage_order_type.clone());
                if matches!(&self.arbitrage_order_type, OrderType::GTD) {
                    b.expiration(expiration).build().await
                } else {
                    b.build().await
                }
            },
            async {
                let b = self
                    .client
                    .limit_order()
                    .token_id(no_token_id)
                    .side(Side::Buy)
                    .price(no_price_with_slippage)
                    .size(order_size)
                    .order_type(self.arbitrage_order_type.clone());
                if matches!(&self.arbitrage_order_type, OrderType::GTD) {
                    b.expiration(expiration).build().await
                } else {
                    b.build().await
                }
            }
        );

        let yes_order = yes_order?;
        let no_order = no_order?;
        let build_elapsed = build_start.elapsed().as_millis();

        let sign_start = Instant::now();
        let signer = LocalSigner::from_str(&self.private_key)?.with_chain_id(Some(POLYGON));

        let (signed_yes_result, signed_no_result) = tokio::join!(
            self.client.sign(&signer, yes_order),
            self.client.sign(&signer, no_order)
        );

        let signed_yes = signed_yes_result?;
        let signed_no = signed_no_result?;
        let sign_elapsed = sign_start.elapsed().as_millis();

        let send_start = Instant::now();

        let (yes_result, no_result) = if concurrent {
            // ===== Concurrent send (FOK/FAK/GTD) =====
            // Fire both legs at once: wall-clock ~= a single round-trip, removing
            // the second-leg delay that let prices move away under sequential
            // gating.
            //
            // Concurrency does NOT eliminate one-sided fills. FOK/FAK residuals
            // are terminal and reconciled by re-hedge/unwind. GTD legs are left
            // resting (no auto-cancel) to fill or expire on their own; any final
            // imbalance is left to the normal position/wind-down lifecycle.
            debug!(
                "Concurrent send ({}); no pre-submit gating | {}",
                self.arbitrage_order_type,
                &pair_id[..8]
            );
            let (yes_r, no_r) = tokio::join!(
                self.client.post_order(signed_yes),
                self.client.post_order(signed_no),
            );
            let yes_r = yes_r.or_else(|e| match Self::ioc_no_match_zero_fill_response(&e) {
                Some(r) => Ok(r),
                None => Err(e),
            });
            let no_r = no_r.or_else(|e| match Self::ioc_no_match_zero_fill_response(&e) {
                Some(r) => Ok(r),
                None => Err(e),
            });
            match (yes_r, no_r) {
                (Ok(y), Ok(n)) => (y, n),
                (Ok(y), Err(e)) => {
                    if Self::is_terminal_ioc_order_type(&self.arbitrage_order_type) {
                        if y.taking_amount > dec!(0) {
                            warn!(
                                "⚠️ NO submit failed but YES filled {} → market-unwind YES | {}",
                                y.taking_amount,
                                &pair_id[..8]
                            );
                            self.market_unwind_leg(yes_token_id, "YES", y.taking_amount, &pair_id)
                                .await;
                        }
                    } else {
                        // GTD: leave the posted YES leg resting (no auto-cancel).
                        // It fills or expires with the market; a transient
                        // one-sided leg is accepted per the resting-GTD strategy.
                        warn!(
                            "⚠️ NO submit failed; leaving GTD YES leg resting (filled {} so far, no cancel) | {}",
                            y.taking_amount,
                            &pair_id[..8]
                        );
                    }
                    return Self::log_send_error(
                        &pair_id,
                        yes_price_with_slippage,
                        no_price_with_slippage,
                        order_size,
                        build_elapsed,
                        sign_elapsed,
                        send_start,
                        total_start,
                        e,
                    );
                }
                (Err(e), Ok(n)) => {
                    if Self::is_terminal_ioc_order_type(&self.arbitrage_order_type) {
                        if n.taking_amount > dec!(0) {
                            warn!(
                                "⚠️ YES submit failed but NO filled {} → market-unwind NO | {}",
                                n.taking_amount,
                                &pair_id[..8]
                            );
                            self.market_unwind_leg(no_token_id, "NO", n.taking_amount, &pair_id)
                                .await;
                        }
                    } else {
                        // GTD: leave the posted NO leg resting (no auto-cancel).
                        // It fills or expires with the market; a transient
                        // one-sided leg is accepted per the resting-GTD strategy.
                        warn!(
                            "⚠️ YES submit failed; leaving GTD NO leg resting (filled {} so far, no cancel) | {}",
                            n.taking_amount,
                            &pair_id[..8]
                        );
                    }
                    return Self::log_send_error(
                        &pair_id,
                        yes_price_with_slippage,
                        no_price_with_slippage,
                        order_size,
                        build_elapsed,
                        sign_elapsed,
                        send_start,
                        total_start,
                        e,
                    );
                }
                (Err(ye), Err(ne)) => {
                    debug!(
                        "Both legs submit-failed | YES:{} | NO:{} | {}",
                        ye,
                        ne,
                        &pair_id[..8]
                    );
                    return Self::log_send_error(
                        &pair_id,
                        yes_price_with_slippage,
                        no_price_with_slippage,
                        order_size,
                        build_elapsed,
                        sign_elapsed,
                        send_start,
                        total_start,
                        ye,
                    );
                }
            }
        } else {
            // ===== Gated sequential send =====
            // NOTE: currently unreachable — FOK/FAK/GTD/GTC all send concurrently
            // (see is_concurrent_order_type). Retained for a potential future order
            // type that must stay gated: fire the pricier leg first, and only fire
            // the second leg once the first has actually filled; a first leg that
            // rests unfilled is cancelled and the second skipped — closing the
            // "cheap leg fills alone, pricier leg misses → one-sided" gap.
            let yes_first = yes_price_with_slippage >= no_price_with_slippage;
            let (first_signed, second_signed, first_side, second_side) = if yes_first {
                (signed_yes, signed_no, "YES", "NO")
            } else {
                (signed_no, signed_yes, "NO", "YES")
            };

            let first_res = match self.client.post_order(first_signed).await {
                Ok(r) => r,
                Err(e) => {
                    return Self::log_send_error(
                        &pair_id,
                        yes_price_with_slippage,
                        no_price_with_slippage,
                        order_size,
                        build_elapsed,
                        sign_elapsed,
                        send_start,
                        total_start,
                        e,
                    );
                }
            };

            if !Self::should_send_second_leg(first_res.taking_amount) {
                // First leg didn't fill immediately. A GTC order rests in the book
                // and could still fill later, re-creating the one-sided risk, so
                // cancel it best-effort and skip the second leg entirely.
                if let Err(e) = self.client.cancel_order(&first_res.order_id).await {
                    warn!(
                        "⚠️ Failed to cancel unfilled first leg ({}) | id:{} | err:{}",
                        first_side, first_res.order_id, e
                    );
                }
                warn!(
                    "⏭️ First leg ({}) unfilled → skip second leg ({}) to avoid one-sided exposure | {}",
                    first_side,
                    second_side,
                    &pair_id[..8]
                );
                return Err(anyhow::anyhow!(
                    "First leg ({}) unfilled; second leg ({}) skipped to avoid one-sided exposure",
                    first_side,
                    second_side
                ));
            }

            let second_res = match self.client.post_order(second_signed).await {
                Ok(r) => r,
                Err(e) => {
                    return Self::log_send_error(
                        &pair_id,
                        yes_price_with_slippage,
                        no_price_with_slippage,
                        order_size,
                        build_elapsed,
                        sign_elapsed,
                        send_start,
                        total_start,
                        e,
                    );
                }
            };

            if yes_first {
                (first_res, second_res)
            } else {
                (second_res, first_res)
            }
        };

        let send_elapsed = send_start.elapsed().as_millis();
        let total_elapsed = total_start.elapsed().as_millis();
        info!(
            "⏱️ Latency | {} | build {}ms sign {}ms send {}ms total {}ms",
            &pair_id[..8],
            build_elapsed,
            sign_elapsed,
            send_elapsed,
            total_elapsed
        );

        // Reconcile if the legs came back unbalanced. Concurrent FOK/FAK fills
        // are terminal, so actively try to complete the under-filled leg
        // (profit-gated) before unwinding. Concurrent GTD legs are left resting
        // (no auto-cancel); we only snapshot their fills and let them work until
        // they fill or the GTD expiry elapses. Sequential GTC uses the original
        // gated path.
        let (yes_filled, no_filled) =
            if Self::is_terminal_ioc_order_type(&self.arbitrage_order_type) {
                self.reconcile_concurrent_rehedge(
                    &pair_id,
                    &yes_result,
                    yes_token_id,
                    yes_price_with_slippage,
                    &no_result,
                    no_token_id,
                    no_price_with_slippage,
                )
                .await
            } else if concurrent {
                self.record_concurrent_resting_fills(&pair_id, &yes_result, &no_result)
                    .await
            } else {
                self.reconcile_pair_after_grace(
                    &pair_id,
                    &yes_result,
                    yes_token_id,
                    &no_result,
                    no_token_id,
                )
                .await
            };

        if yes_filled == dec!(0) && no_filled == dec!(0) {
            let yes_error_msg = yes_result.error_msg.as_deref().unwrap_or("unknown error");
            let no_error_msg = no_result.error_msg.as_deref().unwrap_or("unknown error");

            let yes_error_simple = if yes_error_msg.contains("no orders found to match") {
                "No matching orders in orderbook"
            } else if yes_error_msg.contains("GTD")
                || yes_error_msg.contains("FOK")
                || yes_error_msg.contains("FAK")
                || yes_error_msg.contains("GTC")
            {
                "Order cannot fill"
            } else {
                yes_error_msg
            };

            let no_error_simple = if no_error_msg.contains("no orders found to match") {
                "No matching orders in orderbook"
            } else if no_error_msg.contains("GTD")
                || no_error_msg.contains("FOK")
                || no_error_msg.contains("FAK")
                || no_error_msg.contains("GTC")
            {
                "Order cannot fill"
            } else {
                no_error_msg
            };

            error!(
                "❌ Arbitrage failed | pair_id:{} | YES:{} | NO:{}",
                &pair_id[..8],
                yes_error_simple,
                no_error_simple
            );

            debug!(
                pair_id = %pair_id,
                yes_order_id = ?yes_result.order_id,
                no_order_id = ?no_result.order_id,
                yes_success = yes_result.success,
                no_success = no_result.success,
                yes_error = %yes_error_msg,
                no_error = %no_error_msg,
                "Both orders unfilled (details)"
            );

            return Err(anyhow::anyhow!(
                "Arbitrage failed: YES and NO orders both unfilled | YES: {}, NO: {}",
                yes_error_simple,
                no_error_simple
            ));
        }

        if !yes_result.success || !no_result.success {
            let yes_error_msg = yes_result.error_msg.as_deref().unwrap_or("unknown error");
            let no_error_msg = no_result.error_msg.as_deref().unwrap_or("unknown error");

            let yes_error_simple = if yes_error_msg.contains("no orders found to match") {
                "Partially unfilled (order posted)"
            } else if yes_error_msg.contains("GTD")
                || yes_error_msg.contains("FOK")
                || yes_error_msg.contains("FAK")
                || yes_error_msg.contains("GTC")
            {
                "Partially unfilled (order posted)"
            } else {
                "Status abnormal"
            };

            let no_error_simple = if no_error_msg.contains("no orders found to match") {
                "Partially unfilled (order posted)"
            } else if no_error_msg.contains("GTD")
                || no_error_msg.contains("FOK")
                || no_error_msg.contains("FAK")
                || no_error_msg.contains("GTC")
            {
                "Partially unfilled (order posted)"
            } else {
                "Status abnormal"
            };

            warn!(
                "⚠️ Partial order status | pair_id:{} | YES:{} (filled:{}) | NO:{} (filled:{}) | risk mgmt triggered",
                &pair_id[..8],
                yes_error_simple,
                yes_filled,
                no_error_simple,
                no_filled
            );

            debug!(
                pair_id = %pair_id,
                yes_order_id = ?yes_result.order_id,
                no_order_id = ?no_result.order_id,
                yes_success = yes_result.success,
                no_success = no_result.success,
                yes_error = %yes_error_msg,
                no_error = %no_error_msg,
                "Order submit status details"
            );
        }

        if yes_filled > dec!(0) && no_filled > dec!(0) {
            info!(
                "✅ Arbitrage success | pair_id:{} | YES filled:{} | NO filled:{} | total:{}",
                &pair_id[..8],
                yes_filled,
                no_filled,
                yes_filled.min(no_filled)
            );
        } else if yes_filled > dec!(0) || no_filled > dec!(0) {
            let side = if yes_filled > dec!(0) { "YES" } else { "NO" };
            let filled = if yes_filled > dec!(0) {
                yes_filled
            } else {
                no_filled
            };
            let other_side = if yes_filled > dec!(0) { "NO" } else { "YES" };
            warn!(
                "⚠️ One-sided fill | {} | {} filled {}, {} unfilled (handed to risk)",
                &pair_id[..8],
                side,
                filled,
                other_side
            );
        } else {
            warn!(
                "❌ Arbitrage failed | pair_id:{} | YES and NO both unfilled",
                &pair_id[..8]
            );
        }

        Ok(OrderPairResult {
            pair_id,
            yes_order_id: yes_result.order_id.clone(),
            no_order_id: no_result.order_id.clone(),
            yes_filled,
            no_filled,
            yes_size: order_size,
            no_size: order_size,
            success: true,
        })
    }

    fn log_send_error(
        pair_id: &str,
        yes_price: Decimal,
        no_price: Decimal,
        order_size: Decimal,
        build_elapsed: u128,
        sign_elapsed: u128,
        send_start: Instant,
        total_start: Instant,
        e: impl std::fmt::Display,
    ) -> Result<OrderPairResult> {
        let send_elapsed = send_start.elapsed().as_millis();
        let total_elapsed = total_start.elapsed().as_millis();
        error!(
            "❌ V2 order API failed | pair_id:{} | YES:{} NO:{} size:{} | build {}ms sign {}ms send {}ms total {}ms | err:{}",
            &pair_id[..8],
            yes_price,
            no_price,
            order_size,
            build_elapsed,
            sign_elapsed,
            send_elapsed,
            total_elapsed,
            e
        );
        Err(anyhow::anyhow!("V2 order API failed: {}", e))
    }

    /// Given both legs' final fills, decide the unwind action. Returns
    /// `Some((unwind_yes, shares))` — `unwind_yes == true` means sell the YES leg
    /// — or `None` when the imbalance is below Polymarket's 5-share minimum order
    /// size and therefore cannot be unwound.
    fn unwind_plan(yes_final: Decimal, no_final: Decimal) -> Option<(bool, Decimal)> {
        let imbalance = ((yes_final - no_final).abs() * dec!(100)).floor() / dec!(100);
        if imbalance < dec!(5) {
            return None;
        }
        Some((yes_final > no_final, imbalance))
    }

    /// Plan the re-hedge top-up for an imbalanced concurrent pair. Returns `None`
    /// when the legs are balanced within the 5-share minimum. Otherwise:
    /// `topup_yes` — buy more YES (true) or NO (false), whichever is under-filled;
    /// `shortfall` — shares needed to balance;
    /// `max_buy_price` — the highest price we can pay on the under-filled leg while
    /// keeping the pair profitable, i.e. `1 - (over-filled leg's entry price)`
    /// (clamped at 0). A completing buy above this locks in a loss, worse than
    /// unwinding, so the caller only tops up below it.
    fn rehedge_target(
        yes_have: Decimal,
        no_have: Decimal,
        yes_price: Decimal,
        no_price: Decimal,
    ) -> Option<(bool, Decimal, Decimal)> {
        let imbalance = ((yes_have - no_have).abs() * dec!(100)).floor() / dec!(100);
        if imbalance < dec!(5) {
            return None;
        }
        let topup_yes = yes_have < no_have;
        // The over-filled (held) leg's entry price bounds how much we can pay to
        // complete the other side and still keep total cost < $1.
        let held_price = if topup_yes { no_price } else { yes_price };
        let max_buy_price = (dec!(1) - held_price).max(dec!(0));
        Some((topup_yes, imbalance, max_buy_price))
    }

    /// Final matched size for an order via CLOB order lookup.
    async fn matched_size(&self, order_id: &str) -> Option<Decimal> {
        self.client
            .order(order_id)
            .await
            .ok()
            .map(|o| o.size_matched)
    }

    /// Matched size for a posted leg WITHOUT cancelling it — GTD legs are left
    /// resting to fill or expire on their own. Falls back to the immediate fill
    /// when there is no order id or the order lookup fails.
    async fn final_matched_or_immediate(
        &self,
        res: &PostOrderResponse,
        side: &str,
        pair_id: &str,
    ) -> Decimal {
        if res.order_id.is_empty() {
            return res.taking_amount;
        }
        match self.matched_size(&res.order_id).await {
            Some(size) => size,
            None => {
                warn!(
                    "⚠️ Could not query matched size for {}; using immediate fill {} | {}",
                    side,
                    res.taking_amount,
                    &pair_id[..8]
                );
                res.taking_amount
            }
        }
    }

    /// Best (highest) bid price for a token, independent of book ordering.
    async fn best_bid(&self, token: U256) -> Option<Decimal> {
        let req = OrderBookSummaryRequest::builder().token_id(token).build();
        let book = self.client.order_book(&req).await.ok()?;
        book.bids
            .iter()
            .map(|lvl| lvl.price)
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// Best (lowest) ask price for a token, independent of book ordering.
    async fn best_ask(&self, token: U256) -> Option<Decimal> {
        let req = OrderBookSummaryRequest::builder().token_id(token).build();
        let book = self.client.order_book(&req).await.ok()?;
        book.asks
            .iter()
            .map(|lvl| lvl.price)
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// Buy `size` shares of `token` at `price` via FAK (take available liquidity,
    /// kill the rest — never rests as a naked order). Best-effort: returns the
    /// shares filled, or 0 if the build/sign/submit fails (logged).
    async fn buy_at_price(&self, token: U256, price: Decimal, size: Decimal) -> Decimal {
        let size = Self::buy_size_for_api_amount_precision(price, size);
        if size < dec!(5) {
            return dec!(0);
        }

        let signer = match LocalSigner::from_str(&self.private_key) {
            Ok(s) => s.with_chain_id(Some(POLYGON)),
            Err(e) => {
                warn!("Re-hedge buy signer failed: {}", e);
                return dec!(0);
            }
        };
        let order = match self
            .client
            .limit_order()
            .token_id(token)
            .side(Side::Buy)
            .price(price)
            .size(size)
            .order_type(OrderType::FAK)
            .build()
            .await
        {
            Ok(o) => o,
            Err(e) => {
                warn!("Re-hedge buy build failed: {}", e);
                return dec!(0);
            }
        };
        let signed = match self.client.sign(&signer, order).await {
            Ok(s) => s,
            Err(e) => {
                warn!("Re-hedge buy sign failed: {}", e);
                return dec!(0);
            }
        };
        match self.client.post_order(signed).await {
            Ok(r) => r.taking_amount,
            Err(e) => {
                warn!("Re-hedge buy submit failed: {}", e);
                dec!(0)
            }
        }
    }

    /// Market-unwind one over-filled leg at best-bid minus 2 ticks. Best-effort:
    /// returns the shares actually sold, or 0 if there's no bid or the sell fails
    /// (logged; the residual is left for wind-down).
    async fn market_unwind_leg(
        &self,
        token: U256,
        side: &str,
        shares: Decimal,
        pair_id: &str,
    ) -> Decimal {
        let bid = match self.best_bid(token).await {
            Some(b) if b > dec!(0) => b,
            _ => {
                warn!(
                    "⚖️ No bid to unwind {} imbalance {}; left for wind-down | {}",
                    side,
                    shares,
                    &pair_id[..8]
                );
                return dec!(0);
            }
        };
        let price = (bid - dec!(0.02)).max(dec!(0.01));
        match self.sell_at_price(token, price, shares).await {
            Ok(r) => {
                info!(
                    "⚖️ Unwound {} imbalance | {} shares @ {:.4} → sold {} | {}",
                    side,
                    shares,
                    price,
                    r.taking_amount,
                    &pair_id[..8]
                );
                r.taking_amount
            }
            Err(e) => {
                warn!(
                    "⚖️ Unwind sell failed | {} shares | err:{} | left for wind-down | {}",
                    shares,
                    e,
                    &pair_id[..8]
                );
                dec!(0)
            }
        }
    }

    /// Fill bookkeeping for concurrent resting orders (currently GTD). Both legs
    /// were fired concurrently and are LEFT RESTING — this neither cancels nor
    /// unwinds anything. It waits the grace window so any immediate crossing has
    /// time to settle, then snapshots each leg's matched size for position
    /// tracking. Any unfilled remainder keeps working until it fills or the GTD
    /// expiry elapses.
    async fn record_concurrent_resting_fills(
        &self,
        pair_id: &str,
        yes_res: &PostOrderResponse,
        no_res: &PostOrderResponse,
    ) -> (Decimal, Decimal) {
        let grace = self.hedge_grace_secs;
        debug!(
            "GTD concurrent orders posted; grace {}s then snapshot fills (left resting, no cancel) | {}",
            grace,
            &pair_id[..8]
        );
        if grace > 0 {
            tokio::time::sleep(Duration::from_secs(grace)).await;
        }

        let (yes_final, no_final) = tokio::join!(
            self.final_matched_or_immediate(yes_res, "YES", pair_id),
            self.final_matched_or_immediate(no_res, "NO", pair_id),
        );

        let residual = ((yes_final - no_final).abs() * dec!(100)).floor() / dec!(100);
        if residual >= dec!(5) {
            warn!(
                "⚖️ GTD concurrent imbalance YES {} / NO {}; legs left resting (no auto-cancel/unwind) | {}",
                yes_final,
                no_final,
                &pair_id[..8]
            );
        }

        (yes_final, no_final)
    }

    /// Scheme B reconciliation for an imbalanced pair. Fast no-op when the legs
    /// are already balanced within the 5-share minimum. Otherwise waits the grace
    /// period (letting GTC rests fill), cancels remainders, and market-unwinds the
    /// over-filled leg at best-bid minus 2 ticks. Best-effort: any query/cancel/
    /// sell failure is logged and the pair is left for wind-down. Returns the
    /// (adjusted) fills for position bookkeeping.
    async fn reconcile_pair_after_grace(
        &self,
        pair_id: &str,
        yes_res: &PostOrderResponse,
        yes_token: U256,
        no_res: &PostOrderResponse,
        no_token: U256,
    ) -> (Decimal, Decimal) {
        let yes_immediate = yes_res.taking_amount;
        let no_immediate = no_res.taking_amount;

        // Fast path: balanced enough that any residual can't be unwound anyway.
        if Self::unwind_plan(yes_immediate, no_immediate).is_none() {
            return (yes_immediate, no_immediate);
        }

        // GTC rests in the book, so give the lagging leg a grace period to fill
        // before reconciling. Concurrent FOK/FAK and GTD never reach here.
        let grace = self.hedge_grace_secs;
        warn!(
            "⚖️ Legs unbalanced (YES {} / NO {}); grace {}s then reconcile | {}",
            yes_immediate,
            no_immediate,
            grace,
            &pair_id[..8]
        );
        if grace > 0 {
            tokio::time::sleep(Duration::from_secs(grace)).await;
        }

        // Cancel any resting remainder on both legs first, so fills stop moving
        // before we read them and unwind.
        let _ = self.client.cancel_order(&yes_res.order_id).await;
        let _ = self.client.cancel_order(&no_res.order_id).await;

        // Read final matched sizes. If either lookup fails we cannot trust the
        // imbalance, so skip the unwind rather than risk mis-selling a good leg.
        let (yes_final, no_final) = match (
            self.matched_size(&yes_res.order_id).await,
            self.matched_size(&no_res.order_id).await,
        ) {
            (Some(y), Some(n)) => (y, n),
            _ => {
                warn!(
                    "⚖️ Could not query final fills; skip unwind to avoid mis-selling | {}",
                    &pair_id[..8]
                );
                return (yes_immediate, no_immediate);
            }
        };

        let (unwind_yes, shares) = match Self::unwind_plan(yes_final, no_final) {
            Some(plan) => plan,
            None => {
                let residual = ((yes_final - no_final).abs() * dec!(100)).floor() / dec!(100);
                if residual > dec!(0) {
                    warn!(
                        "⚖️ Residual imbalance {} < 5 shares, cannot unwind (min order size); left for wind-down | {}",
                        residual,
                        &pair_id[..8]
                    );
                }
                return (yes_final, no_final);
            }
        };

        let over_token = if unwind_yes { yes_token } else { no_token };
        let over_side = if unwind_yes { "YES" } else { "NO" };
        let sold = self
            .market_unwind_leg(over_token, over_side, shares, pair_id)
            .await;
        if unwind_yes {
            (yes_final - sold, no_final)
        } else {
            (yes_final, no_final - sold)
        }
    }

    /// Reconciliation for concurrent (FOK/FAK) legs. Their fills are terminal, so a
    /// passive grace-wait would only lengthen naked exposure. Instead: if the legs
    /// came back imbalanced, spend up to `hedge_grace_secs` actively topping up the
    /// under-filled leg — but only while it can fill below the profitability ceiling
    /// (`1 - held leg price`), so we never complete the pair at a loss. If the
    /// window expires still imbalanced, market-unwind the over-filled leg (fall back
    /// to scheme B). Returns the (adjusted) fills for position bookkeeping.
    #[allow(clippy::too_many_arguments)]
    async fn reconcile_concurrent_rehedge(
        &self,
        pair_id: &str,
        yes_res: &PostOrderResponse,
        yes_token: U256,
        yes_price: Decimal,
        no_res: &PostOrderResponse,
        no_token: U256,
        no_price: Decimal,
    ) -> (Decimal, Decimal) {
        let mut yes_have = yes_res.taking_amount;
        let mut no_have = no_res.taking_amount;

        // Balanced within the 5-share minimum → nothing actionable.
        if Self::rehedge_target(yes_have, no_have, yes_price, no_price).is_none() {
            return (yes_have, no_have);
        }

        warn!(
            "⚖️ FAK legs unbalanced (YES {} / NO {}); profit-gated re-hedge up to {}s | {}",
            yes_have,
            no_have,
            self.hedge_grace_secs,
            &pair_id[..8]
        );

        let start = Instant::now();
        let window = Duration::from_secs(self.hedge_grace_secs);
        let poll = Duration::from_secs(1);

        while let Some((topup_yes, shortfall, max_buy_price)) =
            Self::rehedge_target(yes_have, no_have, yes_price, no_price)
        {
            if start.elapsed() >= window {
                break;
            }
            // The held leg is priced out — completing would cost >= $1, a loss.
            if max_buy_price <= dec!(0) {
                break;
            }
            // Sub-$1 notional can't be ordered on Polymarket → give up topping up.
            if max_buy_price * shortfall <= dec!(1) {
                break;
            }

            let under_token = if topup_yes { yes_token } else { no_token };
            match self.best_ask(under_token).await {
                // Profitable liquidity exists → take it up to our price ceiling.
                Some(ask) if ask < max_buy_price => {
                    let bought = self
                        .buy_at_price(under_token, max_buy_price, shortfall)
                        .await;
                    if bought > dec!(0) {
                        if topup_yes {
                            yes_have += bought;
                        } else {
                            no_have += bought;
                        }
                        info!(
                            "⚖️ Re-hedge +{} {} @≤{:.4} | {}",
                            bought,
                            if topup_yes { "YES" } else { "NO" },
                            max_buy_price,
                            &pair_id[..8]
                        );
                        continue; // re-evaluate the imbalance immediately after a fill
                    }
                    tokio::time::sleep(poll).await;
                }
                // No profitable liquidity yet → wait and re-check within the window.
                _ => tokio::time::sleep(poll).await,
            }
        }

        // Still imbalanced after the window → unwind the over-filled leg.
        match Self::unwind_plan(yes_have, no_have) {
            Some((unwind_yes, shares)) => {
                let (over_token, over_side) = if unwind_yes {
                    (yes_token, "YES")
                } else {
                    (no_token, "NO")
                };
                let sold = self
                    .market_unwind_leg(over_token, over_side, shares, pair_id)
                    .await;
                if unwind_yes {
                    (yes_have - sold, no_have)
                } else {
                    (yes_have, no_have - sold)
                }
            }
            None => (yes_have, no_have),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polymarket_client_sdk_v2::clob::types::OrderStatusType;
    use polymarket_client_sdk_v2::error::{Error, Method, StatusCode};

    #[test]
    fn capped_order_size_uses_smallest_available_size_and_runtime_cap() {
        assert_eq!(
            TradingExecutor::capped_order_size(dec!(50), dec!(40), dec!(25)),
            dec!(25)
        );
        assert_eq!(
            TradingExecutor::capped_order_size(dec!(12), dec!(40), dec!(25)),
            dec!(12)
        );
        assert_eq!(
            TradingExecutor::capped_order_size(dec!(50), dec!(9), dec!(25)),
            dec!(9)
        );
    }

    #[test]
    fn second_leg_only_sent_when_first_leg_filled() {
        // No immediate fill on the first leg → do not send the second leg,
        // preventing a one-sided (naked) position.
        assert!(!TradingExecutor::should_send_second_leg(dec!(0)));
        // Any positive immediate fill → proceed with the hedging second leg.
        assert!(TradingExecutor::should_send_second_leg(dec!(0.01)));
        assert!(TradingExecutor::should_send_second_leg(dec!(10)));
    }

    #[test]
    fn unwind_plan_targets_the_over_filled_leg_above_min_size() {
        // Balanced → nothing to unwind.
        assert_eq!(TradingExecutor::unwind_plan(dec!(10), dec!(10)), None);
        // Imbalance below the 5-share minimum → cannot unwind.
        assert_eq!(TradingExecutor::unwind_plan(dec!(10), dec!(7)), None);
        // YES over-filled by >= 5 → sell YES for the difference.
        assert_eq!(
            TradingExecutor::unwind_plan(dec!(10), dec!(3)),
            Some((true, dec!(7)))
        );
        // NO over-filled by >= 5 → sell NO for the difference.
        assert_eq!(
            TradingExecutor::unwind_plan(dec!(2), dec!(10)),
            Some((false, dec!(8)))
        );
    }

    #[test]
    fn concurrent_send_covers_all_order_types() {
        // FOK/FAK are terminal; GTD/GTC are left resting with no auto-cancel
        // (GTD via expiry, GTC via cancel_all at window switch). All four send
        // both legs concurrently.
        assert!(TradingExecutor::is_concurrent_order_type(&OrderType::FOK));
        assert!(TradingExecutor::is_concurrent_order_type(&OrderType::FAK));
        assert!(TradingExecutor::is_concurrent_order_type(&OrderType::GTD));
        assert!(TradingExecutor::is_concurrent_order_type(&OrderType::GTC));
    }

    #[test]
    fn ioc_no_match_status_error_becomes_zero_fill_response() {
        let error_msg = "no orders found to match with FAK order. FAK orders are partially filled or killed if no match is found.";
        let order_id = "0x4653ad7380243091897d557c0cae43e475ea817d588a9ebc018bbba5cb5ce9aa";
        let err = Error::status(
            StatusCode::BAD_REQUEST,
            Method::POST,
            "/order".to_string(),
            format!(r#"{{"error":"{}","orderID":"{}"}}"#, error_msg, order_id),
        );

        let response = TradingExecutor::ioc_no_match_zero_fill_response(&err)
            .expect("FAK/FOK no-match status should normalize to zero fill");

        assert_eq!(response.error_msg.as_deref(), Some(error_msg));
        assert_eq!(response.making_amount, dec!(0));
        assert_eq!(response.taking_amount, dec!(0));
        assert_eq!(response.order_id, order_id);
        assert_eq!(response.status, OrderStatusType::Unmatched);
        assert!(!response.success);
    }

    #[test]
    fn buy_pair_size_quantizes_notional_to_market_buy_precision() {
        let adjusted = TradingExecutor::buy_pair_size_for_api_amount_precision(
            dec!(0.41),
            dec!(0.53),
            dec!(9.4),
        );

        assert_eq!(adjusted, dec!(9));
        assert_eq!((dec!(0.41) * adjusted).normalize().scale(), 2);
        assert_eq!((dec!(0.53) * adjusted).normalize().scale(), 2);
    }

    #[test]
    fn rehedge_target_tops_up_under_filled_leg_with_profit_ceiling() {
        // Balanced within 5 shares → no re-hedge.
        assert_eq!(
            TradingExecutor::rehedge_target(dec!(10), dec!(10), dec!(0.5), dec!(0.5)),
            None
        );
        assert_eq!(
            TradingExecutor::rehedge_target(dec!(10), dec!(7), dec!(0.5), dec!(0.5)),
            None
        );
        // YES under-filled (NO over-filled at 0.55) → buy YES for the 7-share
        // shortfall, paying at most 1 - 0.55 = 0.45 to stay profitable.
        assert_eq!(
            TradingExecutor::rehedge_target(dec!(3), dec!(10), dec!(0.40), dec!(0.55)),
            Some((true, dec!(7), dec!(0.45)))
        );
        // NO under-filled (YES over-filled at 0.60) → buy NO for the 8-share
        // shortfall, ceiling 1 - 0.60 = 0.40.
        assert_eq!(
            TradingExecutor::rehedge_target(dec!(10), dec!(2), dec!(0.60), dec!(0.30)),
            Some((false, dec!(8), dec!(0.40)))
        );
        // Held leg already >= $1 → ceiling clamps to 0 (completing would be a loss).
        assert_eq!(
            TradingExecutor::rehedge_target(dec!(2), dec!(10), dec!(0.5), dec!(1.0)),
            Some((true, dec!(8), dec!(0)))
        );
    }
}
