use crate::monitor::MarketSnapshot;
use rust_decimal::Decimal;
use std::sync::Arc;
use tokio::sync::Mutex;
use log::info;

/// Detector for $0.99 trigger strategy (using ASK price)
pub struct PriceDetector {
    trigger_price: f64, // $0.99 (ASK price - what we receive when selling)
    min_time_remaining_seconds: u64, // 90 seconds
    current_period_bought: Arc<Mutex<Option<u64>>>, // Track which period we bought in
}

#[derive(Debug, Clone)]
pub struct BuyOpportunity {
    pub eth_condition_id: String,
    pub token_to_buy_id: String, // The token we'll buy (opposite of the higher token)
    pub token_to_buy_price: f64, // Price of the token we'll buy
    pub difference_d: f64, // D = ETH higher price - BTC corresponding price
    pub period_timestamp: u64,
    pub time_remaining_seconds: u64,
}

impl PriceDetector {
    pub fn new(trigger_price: f64, min_time_remaining_seconds: u64) -> Self {
        Self {
            trigger_price,
            min_time_remaining_seconds,
            current_period_bought: Arc::new(Mutex::new(None)),
        }
    }

    /// Check if ETH higher token ASK price hits $0.99 and calculate D
    /// Returns Some(BuyOpportunity) if conditions are met
    pub async fn detect_opportunity(&self, snapshot: &MarketSnapshot) -> Option<BuyOpportunity> {
        // Reject expired markets (time_remaining_seconds <= 0)
        if snapshot.time_remaining_seconds == 0 {
            return None;
        }
        
        // Check if we already bought in this period
        let period_bought = {
            let bought = self.current_period_bought.lock().await;
            *bought
        };
        
        if period_bought == Some(snapshot.period_timestamp) {
            // Already bought in this period, don't buy again
            return None;
        }

        // Get ETH market prices
        let eth_up = snapshot.eth_market.up_token.as_ref()?;
        let eth_down = snapshot.eth_market.down_token.as_ref()?;
        
        // Use ASK price for buy trigger (ASK = what we receive when selling, lower)
        let eth_up_price = decimal_to_f64(eth_up.ask.unwrap_or(rust_decimal::Decimal::ZERO));
        let eth_down_price = decimal_to_f64(eth_down.ask.unwrap_or(rust_decimal::Decimal::ZERO));

        // Determine which ETH token is higher
        let (eth_higher_price, eth_higher_is_up) = if eth_up_price > eth_down_price {
            (eth_up_price, true)
        } else {
            (eth_down_price, false)
        };

        // Check if ETH higher token hit trigger price ($0.99 ASK price)
        if eth_higher_price < self.trigger_price {
            return None;
        }

        // Get BTC market prices (ASK = what we receive when selling, lower)
        let btc_up = snapshot.btc_market.up_token.as_ref()?;
        let btc_down = snapshot.btc_market.down_token.as_ref()?;
        
        let btc_up_price = decimal_to_f64(btc_up.ask.unwrap_or(rust_decimal::Decimal::ZERO));
        let btc_down_price = decimal_to_f64(btc_down.ask.unwrap_or(rust_decimal::Decimal::ZERO));

        // Calculate D based on which ETH token hit $0.99 (using ASK prices for comparison)
        let difference_d = if eth_higher_is_up {
            // ETH Up hit $0.99 (ASK), compare with BTC Up (ASK)
            eth_up_price - btc_up_price
        } else {
            // ETH Down hit $0.99 (ASK), compare with BTC Down (ASK)
            eth_down_price - btc_down_price
        };

        // D must be >= 0.01 to buy
        if difference_d < 0.01 {
            return None;
        }

        // Time remaining check only applies to $0.01 ≤ D < $0.06
        if difference_d >= 0.01 && difference_d < 0.06 {
            if snapshot.time_remaining_seconds <= self.min_time_remaining_seconds {
                return None;
            }
        }

        // Buy the OPPOSITE token of the one that hit $0.99
        // If ETH Up hit $0.99, buy ETH Down (cheap, can go up)
        // If ETH Down hit $0.99, buy ETH Up (cheap, can go up)
        // Use ASK price for the token we're buying (what we'd receive if selling, but we're buying so this is the lower price)
        let (token_to_buy_id, token_to_buy_price) = if eth_higher_is_up {
            // ETH Up hit $0.99, buy ETH Down
            (eth_down.token_id.clone(), eth_down_price)
        } else {
            // ETH Down hit $0.99, buy ETH Up
            (eth_up.token_id.clone(), eth_up_price)
        };

        let higher_token_name = if eth_higher_is_up { "ETH Up" } else { "ETH Down" };
        let buy_token_name = if eth_higher_is_up { "ETH Down" } else { "ETH Up" };

        crate::log_println!("🎯 Trigger detected! {} ASK price: ${:.4}, D = {:.4}", 
              higher_token_name, eth_higher_price, difference_d);
        crate::log_println!("   Will buy {} token (opposite of higher token)", buy_token_name);
        if difference_d >= 0.01 && difference_d < 0.06 {
            crate::log_println!("   Time remaining: {}s (required: >{}s)", 
                  snapshot.time_remaining_seconds, self.min_time_remaining_seconds);
        } else {
            crate::log_println!("   Time remaining: {}s (no time constraint for this D range)", 
                  snapshot.time_remaining_seconds);
        }

        Some(BuyOpportunity {
            eth_condition_id: snapshot.eth_market.condition_id.clone(),
            token_to_buy_id,
            token_to_buy_price,
            difference_d,
            period_timestamp: snapshot.period_timestamp,
            time_remaining_seconds: snapshot.time_remaining_seconds,
        })
    }

    /// Mark that we bought in this period
    pub async fn mark_period_bought(&self, period_timestamp: u64) {
        let mut bought = self.current_period_bought.lock().await;
        *bought = Some(period_timestamp);
    }

    /// Reset when new period starts
    pub async fn reset_period(&self) {
        let mut bought = self.current_period_bought.lock().await;
        *bought = None;
    }
}

// Helper function for Decimal to f64 conversion
fn decimal_to_f64(d: Decimal) -> f64 {
    d.to_string().parse().unwrap_or(0.0)
}
