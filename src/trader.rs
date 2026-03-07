use crate::api::PolymarketApi;
use crate::models::*;
use crate::detector::BuyOpportunity;
use crate::config::TradingConfig;
use anyhow::Result;
use log::{info, warn, debug};
use std::sync::Arc;
use tokio::sync::Mutex;
use std::collections::HashMap;

/// Calculate sell strategy based on D value
/// Returns (sell_points, sell_percentages)
fn calculate_sell_strategy(d: f64) -> (Vec<f64>, Vec<f64>) {
    if d >= 0.01 && d < 0.06 {
        // Sell all at $0.03
        (vec![0.03], vec![1.0])
    } else if d >= 0.06 && d < 0.1 {
        // Sell half at $0.03, rest at $0.05
        (vec![0.03, 0.05], vec![0.5, 1.0])
    } else if d >= 0.1 && d < 0.2 {
        // Sell half at $0.03, half of rest at $0.05, all rest at $0.07
        (vec![0.03, 0.05, 0.07], vec![0.5, 0.5, 1.0])
    } else if d >= 0.2 && d < 0.35 {
        // Sell half at $0.03, half of rest at $0.05, half of rest at $0.07, all rest at $0.10
        (vec![0.03, 0.05, 0.07, 0.10], vec![0.5, 0.5, 0.5, 1.0])
    } else if d >= 0.35 && d < 0.7 {
        // Sell half at $0.03, half of rest at $0.05, half of rest at $0.07, half of rest at $0.10, all rest at $0.15
        (vec![0.03, 0.05, 0.07, 0.10, 0.15], vec![0.5, 0.5, 0.5, 0.5, 1.0])
    } else if d >= 0.7 && d < 0.8 {
        // Sell half at $0.03, half of rest at $0.05, all rest at $0.07
        (vec![0.03, 0.05, 0.07], vec![0.5, 0.5, 1.0])
    } else if d >= 0.8 && d < 0.85 {
        // Sell half at $0.03, all rest at $0.05
        (vec![0.03, 0.05], vec![0.5, 1.0])
    } else {
        // d >= 0.85: Sell all at $0.03
        (vec![0.03], vec![1.0])
    }
}

pub struct Trader {
    api: Arc<PolymarketApi>,
    config: TradingConfig,
    simulation_mode: bool,
    total_profit: Arc<Mutex<f64>>,
    trades_executed: Arc<Mutex<u64>>,
    pending_trades: Arc<Mutex<HashMap<String, PendingTrade>>>, // Key: period_timestamp
}

impl Trader {
    pub fn new(api: Arc<PolymarketApi>, config: TradingConfig, simulation_mode: bool) -> Self {
        Self {
            api,
            config,
            simulation_mode,
            total_profit: Arc::new(Mutex::new(0.0)),
            trades_executed: Arc::new(Mutex::new(0)),
            pending_trades: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Execute buy when opportunity is detected
    pub async fn execute_buy(&self, opportunity: &BuyOpportunity) -> Result<()> {
        let fixed_amount = self.config.fixed_trade_amount;
        
        // Get current BID price for the token we're buying (what we pay to buy)
        // get_price(token_id, "BUY") returns BID price (what we pay, higher)
        // let price_result = self.api.get_price(&opportunity.token_to_buy_id, "BUY").await;
        // let price = match price_result {
        //     Ok(p) => p,
        //     Err(e) => {
        //         warn!("⚠️  Could not fetch BID price for token: {}", e);
        //         return Err(e);
        //     }
        // };
        
        // let price_f64 = f64::try_from(price).unwrap_or(0.0);
        let price_f64 = opportunity.token_to_buy_price;
        
        // Safety check: reject if price is 0 or too low (market likely expired)
        if price_f64 <= 0.0 || price_f64 < 0.001 {
            warn!("⚠️  Invalid price for token: ${:.4} (market may be expired)", price_f64);
            return Err(anyhow::anyhow!("Invalid price: ${:.4}", price_f64));
        }
        
        let units = fixed_amount / price_f64;
        
        // Calculate sell strategy based on D
        let (sell_points, sell_percentages) = calculate_sell_strategy(opportunity.difference_d);
        
        crate::log_println!("💰 Buying token (opposite of higher ETH token)");
        crate::log_println!("   BID Price (what we pay): ${:.4} | Investment: ${:.2} | Units: {:.2}", 
              price_f64, fixed_amount, units);
        crate::log_println!("   D = {:.4} | Sell strategy: {:?} at {:?}", 
              opportunity.difference_d, sell_percentages, sell_points);
        
        if self.simulation_mode {
            crate::log_println!("   ✅ SIMULATION: Trade executed");
        } else {
            // Place real market order
            let units_rounded = (units * 10000.0).round() / 10000.0;
            
            match self.api.place_market_order(
                &opportunity.token_to_buy_id,
                units_rounded,
                price_f64,
                "BUY",
                Some("FOK"),
            ).await {
                Ok(response) => {
                    crate::log_println!("   ✅ Order placed: {:?}", response);
                }
                Err(e) => {
                    warn!("   ⚠️  Failed to place order: {}", e);
                    return Err(e);
                }
            }
        }
        
        // Track the trade
        let token_id = opportunity.token_to_buy_id.clone();
        crate::log_println!("   📝 Storing trade: Token ID: {}", &token_id[..16]);
        
        let trade = PendingTrade {
            token_id: token_id.clone(),
            condition_id: opportunity.eth_condition_id.clone(),
            investment_amount: fixed_amount,
            total_units: units,
            remaining_units: units,
            purchase_price: price_f64,
            difference_d: opportunity.difference_d,
            timestamp: std::time::Instant::now(),
            market_timestamp: opportunity.period_timestamp,
            sell_points,
            sell_percentages,
            next_sell_index: 0,
        };
        
        let trade_key = opportunity.period_timestamp.to_string();
        let mut pending = self.pending_trades.lock().await;
        pending.insert(trade_key, trade);
        drop(pending);
        
        crate::log_println!("   ✅ Trade stored successfully. Will monitor token price for sell points.");
        
        let mut trades = self.trades_executed.lock().await;
        *trades += 1;
        drop(trades);
        
        Ok(())
    }

    /// Check pending trades and sell at appropriate price points
    pub async fn check_pending_trades(&self) -> Result<()> {
        let pending_trades: Vec<(String, PendingTrade)> = {
            let pending = self.pending_trades.lock().await;
            pending.iter()
                .map(|(key, trade)| (key.clone(), trade.clone()))
                .collect()
        };

        for (key, mut trade) in pending_trades {
            // Skip if all units are sold
            if trade.remaining_units <= 0.0 || trade.next_sell_index >= trade.sell_points.len() {
                continue;
            }

            let current_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            
            // Calculate time remaining for this market
            let market_end_timestamp = trade.market_timestamp + 900; // 15 minutes = 900 seconds
            let time_remaining_seconds = if market_end_timestamp > current_time {
                market_end_timestamp - current_time
            } else {
                0
            };
            
            // Check if time remaining is less than min_time_remaining_seconds
            if time_remaining_seconds <= self.config.min_time_remaining_seconds && trade.remaining_units > 0.0 {
                crate::log_println!("⏰ Time remaining: {}s <= {}s - Canceling pending orders and selling at $0.03", 
                      time_remaining_seconds, self.config.min_time_remaining_seconds);
                
                // Cancel any pending orders first
                if let Err(e) = self.cancel_pending_orders(&trade.token_id).await {
                    warn!("Error canceling pending orders for token {}: {}", &trade.token_id[..16], e);
                }

                // Sell all remaining units at $0.01 (ASK price)
                let sell_price = 0.01;
                crate::log_println!("💰 Selling all remaining {:.2} units at ASK price ${:.4}", 
                      trade.remaining_units, sell_price);
                
                let remaining_units = trade.remaining_units;
                if let Err(e) = self.execute_sell(&key, &mut trade, remaining_units, sell_price).await {
                    warn!("Error selling remaining units: {}", e);
                } else {
                    // Update trade state - all units sold
                    trade.remaining_units = 0.0;
                    trade.next_sell_index = trade.sell_points.len(); // Mark as fully sold
                    
                    // Update in HashMap
                    let mut pending = self.pending_trades.lock().await;
                    if let Some(t) = pending.get_mut(&key) {
                        *t = trade;
                    }
                    drop(pending);
                }
                
                continue; // Skip normal sell point checking for this trade
            }
            
            // Get current ASK price (what we receive when selling - SELL side)
            // get_price(token_id, "SELL") returns ASK price (what we get when selling, lower)
            let price_result = self.api.get_price(&trade.token_id, "SELL").await;
            let current_ask_price = match price_result {
                Ok(p) => {
                    let price_f64 = f64::try_from(p).unwrap_or(0.0);
                    debug!("Checking token {} ASK price: ${:.4} (purchased at BID ${:.4})", 
                           &trade.token_id[..16], price_f64, trade.purchase_price);
                    price_f64
                },
                Err(e) => {
                    debug!("Failed to get ASK price for token {}: {}", &trade.token_id[..16], e);
                    continue; // Skip if can't get price
                }
            };
            
            // Check if we've reached the next sell point (using ASK price - what we'll receive)
            let next_sell_point = trade.sell_points[trade.next_sell_index];
            
            debug!("Token ASK price: ${:.4}, next sell point: ${:.4}, remaining units: {:.2}", 
                   current_ask_price, next_sell_point, trade.remaining_units);
            
            if current_ask_price >= next_sell_point {
                // Calculate how much to sell
                let sell_percentage = trade.sell_percentages[trade.next_sell_index];
                let units_to_sell = if trade.next_sell_index == 0 {
                    // First sell: sell percentage of total
                    trade.total_units * sell_percentage
                } else {
                    // Subsequent sells: sell percentage of remaining
                    trade.remaining_units * sell_percentage
                };
                
                crate::log_println!("📈 Sell point reached! Token ASK price: ${:.4} >= ${:.4}", 
                      current_ask_price, next_sell_point);
                crate::log_println!("   Token ID: {} | Selling {:.2} units ({:.1}%) at ASK price ${:.4}", 
                      &trade.token_id[..16], units_to_sell, sell_percentage * 100.0, current_ask_price);
                
                // Execute sell using ASK price (what we'll receive)
                if let Err(e) = self.execute_sell(&key, &mut trade, units_to_sell, current_ask_price).await {
                    warn!("Error executing sell: {}", e);
                    continue;
                }
                
                // Update trade state
                trade.remaining_units -= units_to_sell;
                trade.next_sell_index += 1;
                
                // Update in HashMap
                let mut pending = self.pending_trades.lock().await;
                if let Some(t) = pending.get_mut(&key) {
                    *t = trade;
                }
                drop(pending);
            }
        }
        
        Ok(())
    }

    /// Cancel any pending orders for a token
    async fn cancel_pending_orders(&self, token_id: &str) -> Result<()> {
        if self.simulation_mode {
            crate::log_println!("   💡 SIMULATION: Canceling pending orders for token {}", &token_id[..16]);
            return Ok(());
        }
        
        crate::log_println!("   🔄 Canceling open orders for token {}", &token_id[..16]);

        // 使用 SDK 的 cancel_market_orders 接口，通过 asset_id 一次性取消该 token 的所有挂单
        if let Err(e) = self.api.cancel_all_open_orders_for_token(token_id).await {
            warn!("   ⚠️  Failed to cancel open orders for token {}: {}", &token_id[..16], e);
            return Err(e);
        }

        crate::log_println!("   ✅ Open orders canceled for token {}", &token_id[..16]);
        Ok(())
    }

    /// Execute sell order
    async fn execute_sell(
        &self,
        _trade_key: &str,
        trade: &PendingTrade,
        units_to_sell: f64,
        current_price: f64,
    ) -> Result<()> {
        if self.simulation_mode {
            let sell_value = current_price * units_to_sell;
            let profit = sell_value - (trade.purchase_price * units_to_sell);
            
            let mut total = self.total_profit.lock().await;
            *total += profit;
            let total_profit = *total;
            drop(total);
            
            crate::log_println!("   💰 SIMULATION: Sold {:.2} units at ${:.4}", units_to_sell, current_price);
            crate::log_println!("   📊 Profit: ${:.4} | Total Profit: ${:.2}", profit, total_profit);
        } else {
            // Place real market sell order
            match self.api.place_market_order(
                &trade.token_id,
                units_to_sell,
                current_price,
                "SELL",
                Some("FOK"),
            ).await {
                Ok(response) => {
                    crate::log_println!("   ✅ Sell order placed: {:?}", response);
                }
                Err(e) => {
                    warn!("   ⚠️  Failed to place sell order: {}", e);
                    return Err(e);
                }
            }
        }
        
        Ok(())
    }

    /// Check and settle trades when markets close
    pub async fn check_market_closure(&self) -> Result<()> {
        let pending_trades: Vec<(String, PendingTrade)> = {
            let pending = self.pending_trades.lock().await;
            pending.iter()
                .map(|(key, trade)| (key.clone(), trade.clone()))
                .collect()
        };
        
        let current_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        for (key, trade) in pending_trades {
            // Market closes at market_timestamp + 900 seconds
            let market_end_timestamp = trade.market_timestamp + 900;
            
            if current_timestamp < market_end_timestamp - 30 {
                continue; // Market hasn't closed yet
            }
            
            // Check if market is closed and redeem if winner
            let (market_closed, is_winner) = self.check_market_result(&trade.condition_id, &trade.token_id).await?;
            
            if market_closed && is_winner && trade.remaining_units > 0.0 {
                // Redeem winning tokens
                if !self.simulation_mode {
                    self.redeem_token(&trade).await;
                }
                
                // Calculate profit from remaining units (worth $1.00 each if winner)
                let remaining_value = trade.remaining_units * 1.0;
                let remaining_cost = trade.purchase_price * trade.remaining_units;
                let profit = remaining_value - remaining_cost;
                
                let mut total = self.total_profit.lock().await;
                *total += profit;
                drop(total);
                
                crate::log_println!("💰 Market Closed - Token Winner: WON | Remaining units: {:.2} | Profit: ${:.4}", 
                      trade.remaining_units, profit);
                
                // Remove trade
                let mut pending = self.pending_trades.lock().await;
                pending.remove(&key);
            }
        }
        
        Ok(())
    }

    async fn check_market_result(&self, condition_id: &str, token_id: &str) -> Result<(bool, bool)> {
        let market = self.api.get_market(condition_id).await?;
        
        let is_closed = market.closed;
        let is_winner = market.tokens.iter()
            .any(|t| t.token_id == token_id && t.winner);
        
        Ok((is_closed, is_winner))
    }

    async fn redeem_token(&self, trade: &PendingTrade) -> Result<()> {
        // Implementation for token redemption
        // This would call the API to redeem winning tokens
        crate::log_println!("🔄 Redeeming {} units of token {}", trade.remaining_units, &trade.token_id[..16]);
        Ok(())
    }

    /// Reset for new period
    pub async fn reset_period(&self, old_period: u64) {
        let mut pending = self.pending_trades.lock().await;
        // Remove trades from old period
        pending.retain(|_, trade| trade.market_timestamp != old_period);
        drop(pending);
    }
}

// Helper trait for Decimal to f64 conversion
trait ToF64 {
    fn to_f64(&self) -> Option<f64>;
}

impl ToF64 for rust_decimal::Decimal {
    fn to_f64(&self) -> Option<f64> {
        self.to_string().parse().ok()
    }
}
