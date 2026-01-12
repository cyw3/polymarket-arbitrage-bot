use crate::models::*;
use crate::monitor::MarketSnapshot;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

#[derive(Clone)]
pub struct ArbitrageDetector {
    min_profit_threshold: Decimal,
    min_time_remaining_to_trade_seconds: u64,
}

impl ArbitrageDetector {
    pub fn new(min_profit_threshold: f64, min_time_remaining_to_trade_seconds: u64) -> Self {
        Self {
            min_profit_threshold: Decimal::from_f64_retain(min_profit_threshold)
                .unwrap_or(dec!(0.01)),
            min_time_remaining_to_trade_seconds,
        }
    }

    /// Detect arbitrage opportunities between ETH and BTC markets
    /// Strategy: Buy Up token in ETH market + Buy Down token in BTC market
    /// when total cost < $1
    pub fn detect_opportunities(&self, snapshot: &MarketSnapshot) -> Vec<ArbitrageOpportunity> {
        let mut opportunities = Vec::new();

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
            ) {
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
            ) {
                opportunities.push(opportunity);
            }
        }

        opportunities
    }

    fn check_arbitrage(
        &self,
        token1: &TokenPrice,
        token2: &TokenPrice,
        condition1: &str,
        condition2: &str,
        strategy: crate::models::ArbitrageStrategy,
    ) -> Option<ArbitrageOpportunity> {
        let price1 = token1.ask_price();
        let price2 = token2.ask_price();
        let total_cost = price1 + price2;
        let dollar = dec!(1.0);
        let min_price_threshold = dec!(0.6);
        let min_higher_token_price = dec!(0.65);

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

