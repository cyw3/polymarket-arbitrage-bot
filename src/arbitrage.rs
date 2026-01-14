use crate::models::*;
use crate::monitor::MarketSnapshot;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use log::{debug, warn};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct ArbitrageDetector {
    min_profit_threshold: Decimal,
    min_time_remaining_to_trade_seconds: u64,
    danger_signal_time_threshold_seconds: u64,
    danger_signal_min_token_price: Decimal,
    // Track if danger signal was triggered for the current period
    // (period_timestamp, danger_triggered)
    danger_signal_state: Arc<Mutex<(u64, bool)>>,
}

impl ArbitrageDetector {
    pub fn new(
        min_profit_threshold: f64,
        min_time_remaining_to_trade_seconds: u64,
        danger_signal_time_threshold_seconds: u64,
        danger_signal_min_token_price: f64,
    ) -> Self {
        Self {
            min_profit_threshold: Decimal::from_f64_retain(min_profit_threshold)
                .unwrap_or(dec!(0.01)),
            min_time_remaining_to_trade_seconds,
            danger_signal_time_threshold_seconds,
            danger_signal_min_token_price: Decimal::from_f64_retain(danger_signal_min_token_price)
                .unwrap_or(dec!(0.45)),
            danger_signal_state: Arc::new(Mutex::new((0, false))),
        }
    }

    /// Reset danger signal flag when a new period starts
    pub async fn reset_period(&self, new_period_timestamp: u64) {
        let mut state = self.danger_signal_state.lock().await;
        *state = (new_period_timestamp, false);
    }

    /// Check if danger signal was already triggered for this period
    async fn is_danger_signal_triggered(&self, current_period_timestamp: u64) -> bool {
        let state = self.danger_signal_state.lock().await;
        state.0 == current_period_timestamp && state.1
    }

    /// Mark danger signal as triggered for the current period
    async fn trigger_danger_signal(&self, current_period_timestamp: u64) {
        let mut state = self.danger_signal_state.lock().await;
        if state.0 != current_period_timestamp {
            // New period, reset
            *state = (current_period_timestamp, true);
        } else {
            // Same period, just mark as triggered
            state.1 = true;
        }
    }

    /// Detect arbitrage opportunities between ETH and BTC markets
    /// Strategy: Buy Up token in ETH market + Buy Down token in BTC market
    /// when total cost < $1
    pub async fn detect_opportunities(&self, snapshot: &MarketSnapshot) -> Vec<ArbitrageOpportunity> {
        let mut opportunities = Vec::new();

        // Check if danger signal was already triggered for this period
        // If yes, block all trading for the rest of this period
        if self.is_danger_signal_triggered(snapshot.period_timestamp).await {
            warn!(
                "🚨 DANGER SIGNAL was triggered earlier for this period ({}). Blocking all trades for the rest of this period.",
                snapshot.period_timestamp
            );
            return opportunities;
        }

        // Filter 1: Only trade when configured time or less remain
        // Default: 600 seconds (10 minutes) - means bot waits 5 minutes before trading
        if snapshot.time_remaining_seconds > self.min_time_remaining_to_trade_seconds {
            // Too early in the period, skip trading
            return opportunities;
        }

        // Get prices from both markets
        let eth_up = snapshot.eth_market.up_token.as_ref();
        let eth_down = snapshot.eth_market.down_token.as_ref();
        let btc_up = snapshot.btc_market.up_token.as_ref();
        let btc_down = snapshot.btc_market.down_token.as_ref();

        // Strategy 1: ETH Up + BTC Down
        if let (Some(eth_up_price), Some(btc_down_price)) = (eth_up, btc_down) {
            if let Some(opportunity) = self.check_arbitrage(
                eth_up_price,
                btc_down_price,
                &snapshot.eth_market.condition_id,
                &snapshot.btc_market.condition_id,
                crate::models::ArbitrageStrategy::EthUpBtcDown,
                snapshot.time_remaining_seconds,
                snapshot.period_timestamp,
            ).await {
                opportunities.push(opportunity);
            }
        }

        // Strategy 2: ETH Down + BTC Up
        if let (Some(eth_down_price), Some(btc_up_price)) = (eth_down, btc_up) {
            if let Some(opportunity) = self.check_arbitrage(
                eth_down_price,
                btc_up_price,
                &snapshot.eth_market.condition_id,
                &snapshot.btc_market.condition_id,
                crate::models::ArbitrageStrategy::EthDownBtcUp,
                snapshot.time_remaining_seconds,
                snapshot.period_timestamp,
            ).await {
                opportunities.push(opportunity);
            }
        }

        opportunities
    }

    async fn check_arbitrage(
        &self,
        token1: &TokenPrice,
        token2: &TokenPrice,
        condition1: &str,
        condition2: &str,
        strategy: crate::models::ArbitrageStrategy,
        time_remaining_seconds: u64,
        period_timestamp: u64,
    ) -> Option<ArbitrageOpportunity> {
        let price1 = token1.ask_price();
        let price2 = token2.ask_price();
        let total_cost = price1 + price2;
        let dollar = dec!(1.0);
        let min_price_threshold = dec!(0.6);
        let min_higher_token_price = dec!(0.65);

        // DANGER SIGNAL: If time remaining < danger_signal_time_threshold_seconds
        // AND both tokens are below danger_signal_min_token_price, reject trade
        // AND mark this period as having a danger signal (blocks all future trades this period)
        if time_remaining_seconds < self.danger_signal_time_threshold_seconds {
            if price1 <= self.danger_signal_min_token_price && price2 <= self.danger_signal_min_token_price {
                // Strong reject - dangerous signal detected
                // Mark this period as having a danger signal
                self.trigger_danger_signal(period_timestamp).await;
                warn!(
                    "🚨 DANGER SIGNAL TRIGGERED: Time remaining {}s < {}s AND both tokens below ${:.2} (Token1: ${:.2}, Token2: ${:.2}) - REJECTING TRADE AND BLOCKING ALL FUTURE TRADES FOR THIS PERIOD",
                    time_remaining_seconds,
                    self.danger_signal_time_threshold_seconds,
                    self.danger_signal_min_token_price,
                    price1,
                    price2
                );
                return None;
            }
        }

        // Safety filter: Don't trade if both tokens are below $0.6 (rug case)
        // This avoids cases where both markets might go against us
        if price1 < min_price_threshold && price2 < min_price_threshold {
            return None;
        }

        // Filter: Higher-priced token must be >= $0.65
        // This ensures we're not trading when both tokens are too cheap
        let higher_price = price1.max(price2);
        if higher_price < min_higher_token_price {
            return None;
        }

        // Check if total cost is less than $1
        if total_cost < dollar {
            let expected_profit = dollar - total_cost;
            
            // Only return if profit meets threshold
            if expected_profit >= self.min_profit_threshold {
                return Some(ArbitrageOpportunity {
                    strategy,
                    eth_token_price: price1,
                    btc_token_price: price2,
                    total_cost,
                    expected_profit,
                    eth_token_id: token1.token_id.clone(),
                    btc_token_id: token2.token_id.clone(),
                    eth_condition_id: condition1.to_string(),
                    btc_condition_id: condition2.to_string(),
                });
            }
        }

        None
    }
}

