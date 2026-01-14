use crate::models::*;
use crate::monitor::MarketSnapshot;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use log::{debug, info};
use std::sync::Arc;
use tokio::sync::Mutex;
use std::time::{Instant, Duration};
use std::collections::VecDeque;

#[derive(Clone)]
struct DataPoint {
    eth_higher_price: Decimal,
    btc_higher_price: Decimal,
    timestamp: Instant,
}

#[derive(Clone)]
pub struct TrendDetector {
    trend_detection_threshold: Decimal,
    duration_analysis_seconds: u64,
    min_passed_data_points: u64,
    min_profit_threshold: Decimal,
    // Track data points for duration analysis
    // (period_timestamp, data_points_queue, analysis_active, collection_started_at)
    analysis_state: Arc<Mutex<(u64, VecDeque<DataPoint>, bool, Option<Instant>)>>,
}

impl TrendDetector {
    pub fn new(
        trend_detection_threshold: f64,
        duration_analysis_seconds: u64,
        min_passed_data_points: u64,
        min_profit_threshold: f64,
    ) -> Self {
        Self {
            trend_detection_threshold: Decimal::from_f64_retain(trend_detection_threshold)
                .unwrap_or(dec!(0.7)),
            duration_analysis_seconds,
            min_passed_data_points,
            min_profit_threshold: Decimal::from_f64_retain(min_profit_threshold)
                .unwrap_or(dec!(0.04)),
            analysis_state: Arc::new(Mutex::new((0, VecDeque::new(), false, None))),
        }
    }

    /// Reset analysis state when a new period starts
    pub async fn reset_period(&self, new_period_timestamp: u64) {
        let mut state = self.analysis_state.lock().await;
        *state = (new_period_timestamp, VecDeque::new(), false, None);
        info!("🔄 Trend detector reset for new period {}", new_period_timestamp);
    }

    /// Detect trend opportunities - same trend trading
    /// Returns Some(opportunity) if conditions are met and duration analysis passes
    pub async fn detect_opportunities(&self, snapshot: &MarketSnapshot) -> Option<TrendOpportunity> {
        // Get prices from both markets
        let eth_up = snapshot.eth_market.up_token.as_ref()?;
        let eth_down = snapshot.eth_market.down_token.as_ref()?;
        let btc_up = snapshot.btc_market.up_token.as_ref()?;
        let btc_down = snapshot.btc_market.down_token.as_ref()?;

        let eth_up_price = eth_up.ask_price();
        let eth_down_price = eth_down.ask_price();
        let btc_up_price = btc_up.ask_price();
        let btc_down_price = btc_down.ask_price();

        // Determine which token is higher in each market
        let (eth_higher_price, eth_higher_token_id, eth_higher_is_up) = 
            if eth_up_price > eth_down_price {
                (eth_up_price, eth_up.token_id.clone(), true)
            } else {
                (eth_down_price, eth_down.token_id.clone(), false)
            };

        let (btc_higher_price, btc_higher_token_id, btc_higher_is_up) = 
            if btc_up_price > btc_down_price {
                (btc_up_price, btc_up.token_id.clone(), true)
            } else {
                (btc_down_price, btc_down.token_id.clone(), false)
            };

        // Check if both markets have the same trend
        // Same trend means: both higher tokens are Up, or both are Down
        if eth_higher_is_up != btc_higher_is_up {
            // Different trends - not a same-trend opportunity
            return None;
        }

        // Determine strategy
        let strategy = if eth_higher_is_up {
            TrendStrategy::BothUp
        } else {
            TrendStrategy::BothDown
        };

        // Step 2: Check if both higher tokens are >= trend_detection_threshold ($0.7)
        let both_above_threshold = eth_higher_price >= self.trend_detection_threshold && 
                                   btc_higher_price >= self.trend_detection_threshold;

        let mut state = self.analysis_state.lock().await;
        
        // If this is a new period, reset
        if state.0 != snapshot.period_timestamp {
            *state = (snapshot.period_timestamp, VecDeque::new(), false, None);
        }

        // Step 3: Batch duration analysis
        let now = Instant::now();
        let (period_ts, data_points, analysis_active, collection_started_at) = &mut *state;

        if !both_above_threshold {
            // Threshold not met - reset if we were collecting
            if *analysis_active {
                debug!("📉 Trend threshold dropped below ${:.2}, resetting analysis", self.trend_detection_threshold);
                *data_points = VecDeque::new();
                *analysis_active = false;
                *collection_started_at = None;
            }
            drop(state);
            return None;
        }

        // Both tokens are >= threshold
        if !*analysis_active {
            // Start new collection
            *analysis_active = true;
            *collection_started_at = Some(now);
            *data_points = VecDeque::new();
            info!("📊 Started duration analysis for period {} (collecting {} data points)", 
                  snapshot.period_timestamp, self.duration_analysis_seconds);
        }

        // Add current data point
        data_points.push_back(DataPoint {
            eth_higher_price,
            btc_higher_price,
            timestamp: now,
        });

        let total_points = data_points.len();
        let target_points = self.duration_analysis_seconds as usize;

        // Check if we've collected enough points
        if total_points < target_points {
            // Still collecting - show progress
            debug!("📈 Collecting data points: {}/{}", total_points, target_points);
            drop(state);
            return None;
        }

        // We have exactly (or more than) target_points - analyze the batch
        // Take exactly the first target_points for analysis
        let mut points_to_analyze = VecDeque::new();
        for _ in 0..target_points {
            if let Some(point) = data_points.pop_front() {
                points_to_analyze.push_back(point);
            } else {
                break;
            }
        }

        // Count how many points passed (both tokens >= threshold)
        let passed_points = points_to_analyze.iter()
            .filter(|point| {
                point.eth_higher_price >= self.trend_detection_threshold &&
                point.btc_higher_price >= self.trend_detection_threshold
            })
            .count();

        // Check if we have enough passed points (using min_passed_data_points)
        let min_required = self.min_passed_data_points as usize;
        let enough_passed = passed_points >= min_required;

        if enough_passed {
            // Enough points passed - reset for next collection
            info!("✅ Duration analysis PASSED: {}/{} points passed (need at least {})", 
                  passed_points, target_points, min_required);
            // Clear collected points and reset state
            // If prices are still >= threshold on next call, we'll start a new collection
            *data_points = VecDeque::new();
            *analysis_active = false;
            *collection_started_at = None;
            drop(state);
            // Continue to trade logic below
        } else {
            // Not enough points passed - reset and keep monitoring
            info!("❌ Duration analysis FAILED: {}/{} points passed (need at least {})", 
                  passed_points, target_points, min_required);
            *data_points = VecDeque::new();
            *analysis_active = false;
            *collection_started_at = None;
            drop(state);
            return None;
        }

        // Step 4: Check profit threshold for both tokens
        // Profit = 1.0 - price (if token wins, it's worth $1.00)
        let eth_profit = dec!(1.0) - eth_higher_price;
        let btc_profit = dec!(1.0) - btc_higher_price;

        if eth_profit < self.min_profit_threshold || btc_profit < self.min_profit_threshold {
            debug!("⏸️  Profit threshold not met: ETH profit {:.2}%, BTC profit {:.2}% (need {:.2}%)",
                   eth_profit * dec!(100), btc_profit * dec!(100), self.min_profit_threshold * dec!(100));
            return None;
        }

        // Step 4: Select the higher-priced token (prefer ETH when prices are equal)
        let (selected_token_id, selected_token_price, selected_condition_id) = 
            if eth_higher_price >= btc_higher_price {
                (eth_higher_token_id.clone(), eth_higher_price, snapshot.eth_market.condition_id.clone())
            } else {
                (btc_higher_token_id.clone(), btc_higher_price, snapshot.btc_market.condition_id.clone())
            };

        info!("✅ Trend opportunity detected!");
        info!("   Strategy: {:?}", strategy);
        info!("   ETH higher token: ${:.2} ({}), BTC higher token: ${:.2} ({})",
              eth_higher_price, if eth_higher_is_up { "Up" } else { "Down" },
              btc_higher_price, if btc_higher_is_up { "Up" } else { "Down" });
        info!("   Selected token: ${:.2} (condition: {})", 
              selected_token_price, &selected_condition_id[..16]);
        info!("   Duration analysis: {}/{} points passed (need at least {})", 
              passed_points, target_points, min_required);

        Some(TrendOpportunity {
            strategy,
            eth_higher_token_price: eth_higher_price,
            btc_higher_token_price: btc_higher_price,
            eth_higher_token_id: eth_higher_token_id.clone(),
            btc_higher_token_id: btc_higher_token_id.clone(),
            eth_condition_id: snapshot.eth_market.condition_id.clone(),
            btc_condition_id: snapshot.btc_market.condition_id.clone(),
            selected_token_id,
            selected_token_price,
            selected_condition_id,
        })
    }
}
