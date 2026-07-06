use anyhow::Result;
use dashmap::DashMap;
use futures::Stream;
use futures::StreamExt;
use polymarket_client_sdk::clob::ws::{types::response::BookUpdate, Client as WsClient};
use polymarket_client_sdk::types::{B256, U256};
use std::collections::HashMap;
use std::pin::Pin;
use tracing::{debug, info};

use crate::market::MarketInfo;

pub const DEFAULT_MAX_ORDERBOOK_PAIR_SKEW_MS: u64 = 200;

/// Shorten B256 for logs: 0x + first 8 hex, e.g. 0xb91126b7..
#[inline]
fn short_b256(b: &B256) -> String {
    let s = format!("{b}");
    if s.len() > 12 {
        format!("{}..", &s[..10])
    } else {
        s
    }
}

/// Shorten U256 for logs: last 8 digits, e.g. ..67033653
#[inline]
fn short_u256(u: &U256) -> String {
    let s = format!("{u}");
    if s.len() > 12 {
        format!("..{}", &s[s.len().saturating_sub(8)..])
    } else {
        s
    }
}

pub struct OrderBookMonitor {
    ws_client: WsClient,
    books: DashMap<U256, BookUpdate>,
    market_map: HashMap<B256, (U256, U256)>, // market_id -> (yes_token_id, no_token_id)
    max_pair_skew_ms: u64,
}

pub struct OrderBookPair {
    pub yes_book: BookUpdate,
    pub no_book: BookUpdate,
    pub market_id: B256,
}

impl OrderBookMonitor {
    pub fn with_max_pair_skew_ms(max_pair_skew_ms: u64) -> Self {
        Self {
            // Use unauthenticated client: orderbook is public, no auth needed
            // Only user data (orders, trades) requires auth
            ws_client: WsClient::default(),
            books: DashMap::new(),
            market_map: HashMap::new(),
            max_pair_skew_ms,
        }
    }

    fn pair_skew_ms(yes_book: &BookUpdate, no_book: &BookUpdate) -> u64 {
        yes_book.timestamp.abs_diff(no_book.timestamp)
    }

    fn pair_is_fresh(&self, market_id: &B256, yes_book: &BookUpdate, no_book: &BookUpdate) -> bool {
        let skew_ms = Self::pair_skew_ms(yes_book, no_book);
        if skew_ms <= self.max_pair_skew_ms {
            return true;
        }

        debug!(
            market_id = short_b256(market_id),
            yes_ts = yes_book.timestamp,
            no_ts = no_book.timestamp,
            skew_ms,
            max_pair_skew_ms = self.max_pair_skew_ms,
            "Skip stale orderbook pair"
        );
        false
    }

    /// Subscribe to new market
    pub fn subscribe_market(&mut self, market: &MarketInfo) -> Result<()> {
        // Record market mapping
        self.market_map
            .insert(market.market_id, (market.yes_token_id, market.no_token_id));

        info!(
            market_id = short_b256(&market.market_id),
            yes = short_u256(&market.yes_token_id),
            no = short_u256(&market.no_token_id),
            "Subscribe to market orderbook"
        );

        Ok(())
    }

    /// Create orderbook subscription stream
    ///
    /// Note: Orderbook uses unauthenticated WebSocket; orderbook data is public.
    /// Only user data (order status, trade history) needs auth.
    pub fn create_orderbook_stream(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<BookUpdate>> + Send + '_>>> {
        // Collect all token_ids to subscribe
        let token_ids: Vec<U256> = self
            .market_map
            .values()
            .flat_map(|(yes, no)| [*yes, *no])
            .collect();

        if token_ids.is_empty() {
            return Err(anyhow::anyhow!("No markets to subscribe"));
        }

        info!(
            token_count = token_ids.len(),
            "Creating orderbook stream (unauthenticated)"
        );

        // subscribe_orderbook does not need auth
        let stream = self.ws_client.subscribe_orderbook(token_ids)?;
        // Convert SDK Error to anyhow::Error
        let stream = stream.map(|result| result.map_err(|e| anyhow::anyhow!("{}", e)));
        Ok(Box::pin(stream))
    }

    /// Handle orderbook update
    pub fn handle_book_update(&self, book: BookUpdate) -> Option<OrderBookPair> {
        // Print top 5 bid/ask (debug)
        if !book.bids.is_empty() {
            let top_bids: Vec<String> = book
                .bids
                .iter()
                .take(5)
                .map(|b| format!("{}@{}", b.size, b.price))
                .collect();
            debug!(
                asset_id = %book.asset_id,
                "Top 5 bids: {}",
                top_bids.join(", ")
            );
        }
        if !book.asks.is_empty() {
            let top_asks: Vec<String> = book
                .asks
                .iter()
                .take(5)
                .map(|a| format!("{}@{}", a.size, a.price))
                .collect();
            debug!(
                asset_id = short_u256(&book.asset_id),
                "Top 5 asks: {}",
                top_asks.join(", ")
            );
        }

        // Update orderbook cache
        self.books.insert(book.asset_id, book.clone());

        // Find which market this token belongs to; either side update returns OrderBookPair for arbitrage
        for (market_id, (yes_token, no_token)) in &self.market_map {
            if book.asset_id == *yes_token {
                if let Some(no_book) = self.books.get(no_token) {
                    if !self.pair_is_fresh(market_id, &book, &no_book) {
                        return None;
                    }
                    return Some(OrderBookPair {
                        yes_book: book.clone(),
                        no_book: no_book.clone(),
                        market_id: *market_id,
                    });
                }
            } else if book.asset_id == *no_token {
                if let Some(yes_book) = self.books.get(yes_token) {
                    if !self.pair_is_fresh(market_id, &yes_book, &book) {
                        return None;
                    }
                    return Some(OrderBookPair {
                        yes_book: yes_book.clone(),
                        no_book: book.clone(),
                        market_id: *market_id,
                    });
                }
            }
        }

        None
    }

    /// Get orderbook if present
    pub fn get_book(&self, token_id: U256) -> Option<BookUpdate> {
        self.books.get(&token_id).map(|b| b.clone())
    }

    /// Clear all subscriptions
    pub fn clear(&mut self) {
        self.books.clear();
        self.market_map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use polymarket_client_sdk::clob::ws::types::response::BookUpdate;
    use polymarket_client_sdk::types::b256;

    fn test_market() -> MarketInfo {
        MarketInfo {
            market_id: b256!("0000000000000000000000000000000000000000000000000000000000000001"),
            slug: "btc-updown-5m-test".to_string(),
            yes_token_id: U256::from(1),
            no_token_id: U256::from(2),
            title: "BTC test market".to_string(),
            end_date: Utc::now(),
            crypto_symbol: "btc".to_string(),
        }
    }

    fn book(asset_id: U256, timestamp: i64) -> BookUpdate {
        serde_json::from_value(serde_json::json!({
            "asset_id": asset_id.to_string(),
            "market": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "timestamp": timestamp.to_string(),
            "bids": [{"price": "0.49", "size": "100"}],
            "asks": [{"price": "0.50", "size": "100"}]
        }))
        .expect("valid book update")
    }

    #[test]
    fn skips_pair_when_cached_counter_leg_is_stale() {
        let market = test_market();
        let mut monitor =
            OrderBookMonitor::with_max_pair_skew_ms(DEFAULT_MAX_ORDERBOOK_PAIR_SKEW_MS);
        monitor.subscribe_market(&market).unwrap();

        assert!(monitor
            .handle_book_update(book(market.yes_token_id, 1_000))
            .is_none());

        let pair = monitor.handle_book_update(book(market.no_token_id, 1_250));

        assert!(
            pair.is_none(),
            "UP/DOWN book timestamps differ by more than the 200ms default guard"
        );
    }

    #[test]
    fn accepts_pair_when_book_timestamps_are_within_default_skew() {
        let market = test_market();
        let mut monitor =
            OrderBookMonitor::with_max_pair_skew_ms(DEFAULT_MAX_ORDERBOOK_PAIR_SKEW_MS);
        monitor.subscribe_market(&market).unwrap();

        assert!(monitor
            .handle_book_update(book(market.yes_token_id, 1_000))
            .is_none());

        let pair = monitor.handle_book_update(book(market.no_token_id, 1_200));

        assert!(
            pair.is_some(),
            "200ms skew is accepted by the default guard"
        );
    }
}
