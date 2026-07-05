use alloy::signers::local::LocalSigner;
use alloy::signers::Signer;
use anyhow::Result;
use chrono::Utc;
use polymarket_client_sdk_v2::clob::types::response::{CancelOrdersResponse, PostOrderResponse};
use polymarket_client_sdk_v2::clob::types::{OrderType, Side};
use polymarket_client_sdk_v2::types::{Decimal, U256};
use polymarket_client_sdk_v2::POLYGON;
use rust_decimal_macros::dec;
use std::str::FromStr;
use std::time::Instant;
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
}

impl TradingExecutor {
    pub fn from_client(
        client: AuthenticatedClobClient,
        private_key: String,
        max_order_size_usdc: f64,
        slippage: [f64; 2],
        gtd_expiration_secs: u64,
        arbitrage_order_type: OrderType,
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

    /// Execute arbitrage: submit YES+NO via sequential post_order (V2); order type from config
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

        let expiry_info = if matches!(self.arbitrage_order_type, OrderType::GTD) {
            format!("expiry:{}s", self.gtd_expiration_secs)
        } else {
            "no expiry".to_string()
        };
        debug!(
            market_id = %opp.market_id,
            profit_pct = %opp.profit_percentage,
            order_type = %self.arbitrage_order_type,
            "Execute arbitrage (V2 sequential, type:{}, {})",
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
        let yes_first = yes_price_with_slippage >= no_price_with_slippage;

        // Gated sequential send: fire the pricier leg first, and only fire the
        // second leg once the first has actually filled. If the first leg gets no
        // immediate fill, cancel any resting remainder and abandon the pair.
        //
        // This closes the "cheap leg fills alone, pricier leg misses → one-sided
        // (naked) position" gap. Previously both legs were sent unconditionally,
        // so whenever the first leg rested unfilled but the second filled, we were
        // left holding a single leg (the recurring "only the cheap side got
        // bought" symptom). Now the second leg is never sent unless the first is
        // confirmed filled.
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
            // First leg didn't fill immediately. A GTC/GTD order rests in the book
            // and could still fill later, re-creating the one-sided risk, so cancel
            // it best-effort and skip the second leg entirely.
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

        let (yes_result, no_result) = if yes_first {
            (first_res, second_res)
        } else {
            (second_res, first_res)
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

        let yes_filled = yes_result.taking_amount;
        let no_filled = no_result.taking_amount;

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
