use crate::api::PolymarketApi;
use crate::models::*;
use crate::config::TradingConfig;
use anyhow::Result;
use log::{info, warn, debug};
use std::sync::Arc;
use tokio::sync::Mutex;
use std::collections::HashMap;
use std::time::{Instant, Duration};

#[derive(Clone)]
struct CachedMarketData {
    market: MarketDetails,
    cached_at: Instant,
}

pub struct Trader {
    api: Arc<PolymarketApi>,
    config: TradingConfig,
    simulation_mode: bool,
    total_profit: Arc<Mutex<f64>>,
    trades_executed: Arc<Mutex<u64>>,
    pending_trades: Arc<Mutex<HashMap<String, PendingTrade>>>, // Key: unique trade ID (token_id + timestamp_nanos)
    opposite_side_trades: Arc<Mutex<HashMap<String, OppositeSideTrade>>>, // Key: unique trade ID
    market_cache: Arc<Mutex<HashMap<String, CachedMarketData>>>, // Key: condition_id, cache for 60 seconds
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
            opposite_side_trades: Arc::new(Mutex::new(HashMap::new())),
            market_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Check and settle pending trades when markets close
    /// This is called periodically and also when a new period starts
    pub async fn check_pending_trades(&self) -> Result<()> {
        let mut pending = self.pending_trades.lock().await;
        let mut to_remove = Vec::new();
        
        // Get current timestamp to check if markets have closed
        let current_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        let pending_count = pending.len();
        if pending_count > 0 {
            debug!("Checking {} pending trades for market closure...", pending_count);
        }
        
        for (key, trade) in pending.iter() {
            // Skip trades that were already sold via emergency sell
            // Their profit/loss was already calculated during the emergency sell
            if trade.sold {
                debug!("Trade {} was already sold, skipping market closure check", key);
                continue;
            }
            
            // Market closes at market_timestamp + 900 seconds (15 minutes)
            // Check if market has closed based on the actual market period end time, not trade execution time
            let market_end_timestamp = trade.market_timestamp + 900;
            
            // Only check if we're past the market closing time (with 30 second buffer for API delays)
            if current_timestamp < market_end_timestamp - 30 {
                let time_until_close = market_end_timestamp - current_timestamp;
                debug!("Trade {} market hasn't closed yet (closes in {}s, at timestamp {}), skipping", 
                       key, time_until_close, market_end_timestamp);
                continue;
            }
            
            let time_since_close = current_timestamp.saturating_sub(market_end_timestamp);
            info!("🔍 Checking market closure for trade {} (market closed {}s ago, period: {})", 
                  key, time_since_close, trade.market_timestamp);
            
            // Check if market is closed (using cached data when possible)
            let (market_closed, is_winner) = self.check_market_result_cached(&trade.condition_id, &trade.token_id).await?;
            
            info!("   Market ({}): closed={}, winner={}", 
                  &trade.condition_id[..16], market_closed, is_winner);
            
            if market_closed {
                // Market closed, redeem winning token and calculate actual profit
                if !self.simulation_mode {
                    // In production mode, redeem winning token (worth $1.00 USDC each)
                    self.redeem_token(&trade).await;
                }
                
                let actual_profit = self.calculate_actual_profit(&trade, is_winner);
                
                let mut total = self.total_profit.lock().await;
                *total += actual_profit;
                let total_profit = *total;
                drop(total);
                
                info!(
                    "💰 Market Closed - Token Winner: {} | Actual Profit: ${:.4} | Total Profit: ${:.2}",
                    if is_winner { "WON" } else { "LOST" },
                    actual_profit,
                    total_profit
                );
                
                to_remove.push(key.clone());
            } else {
                info!("   ⏳ Market not closed yet, will check again...");
            }
        }
        
        for key in to_remove {
            pending.remove(&key);
        }
        
        Ok(())
    }

    /// Check previous period's markets when a new period starts
    /// This is called immediately when a new 15-minute period is detected
    /// It checks all pending trades from previous periods and redeems if markets are closed
    pub async fn check_previous_period_markets(&self, _previous_eth_condition_id: &str, _previous_btc_condition_id: &str) -> Result<()> {
        info!("🔍 Checking previous period's markets for closure...");
        
        // Get all pending trades and check which ones are from previous periods
        let trades_to_check: Vec<(String, PendingTrade)> = {
            let pending = self.pending_trades.lock().await;
            pending.iter()
                .filter(|(_, _trade)| {
                    // Check if this trade is from a previous period (not current)
                    // We'll check all trades that might be from previous periods
                    true // Check all pending trades
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        
        if trades_to_check.is_empty() {
            debug!("   No pending trades found");
            return Ok(());
        }
        
        info!("   Found {} pending trade(s) to check", trades_to_check.len());
        
        let mut to_remove = Vec::new();
        
        for (key, trade) in trades_to_check {
            // Skip trades that were already sold via emergency sell
            // Their profit/loss was already calculated during the emergency sell
            if trade.sold {
                debug!("Trade {} was already sold, skipping market closure check", &key[..16]);
                to_remove.push(key); // Remove from pending trades
                continue;
            }
            
            // Check if market is closed (force fresh check, don't use cache)
            let (market_closed, is_winner) = self.check_market_result(&trade.condition_id, &trade.token_id).await?;
            
            info!("   Trade {} - Market ({}): closed={}, winner={}", 
                  &key[..16], &trade.condition_id[..16], market_closed, is_winner);
            
            if market_closed {
                // Market closed, redeem immediately
                info!("✅ Market is closed! Redeeming token...");
                
                if !self.simulation_mode {
                    self.redeem_token(&trade).await;
                }
                
                let actual_profit = self.calculate_actual_profit(&trade, is_winner);
                
                let mut total = self.total_profit.lock().await;
                *total += actual_profit;
                let total_profit = *total;
                drop(total);
                
                info!(
                    "💰 Previous Period Closed - Token Winner: {} | Actual Profit: ${:.4} | Total Profit: ${:.2}",
                    if is_winner { "WON" } else { "LOST" },
                    actual_profit,
                    total_profit
                );
                
                to_remove.push(key);
            } else {
                info!("   ⏳ Market not closed yet, will continue checking...");
            }
        }
        
        // Remove closed trades
        if !to_remove.is_empty() {
            let mut pending = self.pending_trades.lock().await;
            for key in to_remove {
                pending.remove(&key);
            }
        }
        
        Ok(())
    }

    /// Check market result without using cache (for immediate checks)
    async fn check_market_result(&self, condition_id: &str, token_id: &str) -> Result<(bool, bool)> {
        match self.api.get_market(condition_id).await {
            Ok(market) => {
                // Update cache
                let mut cache = self.market_cache.lock().await;
                cache.insert(condition_id.to_string(), CachedMarketData {
                    market: market.clone(),
                    cached_at: Instant::now(),
                });
                drop(cache);
                
                if market.closed {
                    // Find our token and check if it's the winner
                    let winner = market.tokens.iter()
                        .find(|t| t.token_id == token_id)
                        .map(|t| t.winner)
                        .unwrap_or(false);
                    Ok((true, winner))
                } else {
                    Ok((false, false))
                }
            }
            Err(e) => {
                warn!("Failed to fetch market {}: {}", condition_id, e);
                Ok((false, false))
            }
        }
    }

    async fn check_market_result_cached(&self, condition_id: &str, token_id: &str) -> Result<(bool, bool)> {
        // Check cache first (cache for 60 seconds)
        let cache_ttl = Duration::from_secs(60);
        let mut cache = self.market_cache.lock().await;
        
        // Check if we have cached data that's still valid
        if let Some(cached) = cache.get(condition_id) {
            if cached.cached_at.elapsed() < cache_ttl {
                // Use cached data
                let market = &cached.market;
                if market.closed {
                    let winner = market.tokens.iter()
                        .find(|t| t.token_id == token_id)
                        .map(|t| t.winner)
                        .unwrap_or(false);
                    debug!("Using cached market data for condition_id: {}", condition_id);
                    return Ok((true, winner));
                } else {
                    debug!("Using cached market data (not closed yet) for condition_id: {}", condition_id);
                    return Ok((false, false));
                }
            }
        }
        
        // Cache miss or expired - fetch from API
        drop(cache);
        match self.api.get_market(condition_id).await {
            Ok(market) => {
                // Update cache
                let mut cache = self.market_cache.lock().await;
                cache.insert(condition_id.to_string(), CachedMarketData {
                    market: market.clone(),
                    cached_at: Instant::now(),
                });
                drop(cache);
                
                if market.closed {
                    // Find our token and check if it's the winner
                    let winner = market.tokens.iter()
                        .find(|t| t.token_id == token_id)
                        .map(|t| t.winner)
                        .unwrap_or(false);
                    Ok((true, winner))
                } else {
                    Ok((false, false))
                }
            }
            Err(e) => {
                warn!("Failed to fetch market {}: {}", condition_id, e);
                Ok((false, false))
            }
        }
    }

    /// Redeem winning token when market closes (production mode only)
    /// 
    /// IMPORTANT: Redeeming is different from selling!
    /// - Selling: Before market resolves, at current market price
    /// - Redeeming: After market resolves, winning tokens redeemed for $1.00 USDC each
    /// 
    /// When market closes, winning tokens can be redeemed directly for USDC at 1:1 ratio.
    /// This is done through the CTF (Conditional Token Framework) redemption process.
    async fn redeem_token(&self, trade: &PendingTrade) {
        // Determine outcome (Up or Down) by checking market data
        let outcome = match self.api.get_market(&trade.condition_id).await {
            Ok(market_details) => {
                // MarketDetails has tokens field which is Vec<MarketToken>
                // Find the winning token and get its outcome
                market_details.tokens
                    .iter()
                    .find(|t| t.token_id == trade.token_id && t.winner)
                    .map(|t| t.outcome.clone())
                    .unwrap_or_else(|| {
                        // Fallback: if we can't find token, try to infer from token_id
                        "Up".to_string()
                    })
            }
            Err(_) => {
                // Fallback: assume "Up" if we can't fetch market
                "Up".to_string()
            }
        };
        
        // Redeem winning token
        match self.api.redeem_tokens(&trade.condition_id, &trade.token_id, &outcome).await {
            Ok(response) => {
                if response.success {
                    info!("✅ Redeemed {:.2} units of token (winner) for ${:.2} USDC", 
                          trade.units, trade.units);
                    if let Some(tx_hash) = response.transaction_hash {
                        info!("   Transaction hash: {}", tx_hash);
                    }
                } else {
                    warn!("⚠️  Redemption returned success=false: {:?}", response.message);
                }
            }
            Err(e) => {
                warn!("⚠️  Failed to redeem token: {}", e);
                warn!("   Note: You may need to redeem manually through Polymarket UI");
            }
        }
    }

    fn calculate_actual_profit(&self, trade: &PendingTrade, is_winner: bool) -> f64 {
        // When market closes:
        // - If token wins: we get $1 per unit
        // - If token loses: we get $0 per unit
        
        let payout_per_unit = if is_winner {
            1.0 // Token won!
        } else {
            0.0 // Token lost - TOTAL LOSS
        };
        
        let total_payout = payout_per_unit * trade.units;
        let actual_profit = total_payout - trade.investment_amount;
        
        if actual_profit < 0.0 {
            warn!("⚠️  LOSS: Token lost! Lost ${:.4} on this trade", -actual_profit);
        }
        
        actual_profit
    }

    /// Execute trend trade - buy single higher-priced token with fixed amount
    /// Note: No cooldown check needed - duration analysis (60 data points) provides natural cooldown
    pub async fn execute_trend_trade(&self, opportunity: &TrendOpportunity, market_timestamp: u64) -> Result<()> {
        if self.simulation_mode {
            self.simulate_trend_trade(opportunity, market_timestamp).await
        } else {
            self.execute_real_trend_trade(opportunity, market_timestamp).await
        }
    }

    async fn simulate_trend_trade(&self, opportunity: &TrendOpportunity, market_timestamp: u64) -> Result<()> {
        info!("🔍 SIMULATION: Trend trading opportunity detected!");
        
        let fixed_amount = self.config.fixed_trade_amount;
        let token_price = f64::try_from(opportunity.selected_token_price).unwrap_or(1.0);
        let units = fixed_amount / token_price;
        
        info!("   Strategy: {:?}", opportunity.strategy);
        info!("   ETH higher token: ${:.2}, BTC higher token: ${:.2}",
              opportunity.eth_higher_token_price, opportunity.btc_higher_token_price);
        info!("   Selected token: ${:.2} (condition: {})",
              token_price, &opportunity.selected_condition_id[..16]);
        info!("   Fixed trade amount: ${:.2}", fixed_amount);
        info!("   Units to purchase: {:.2} shares", units);
        
        // Track the trade with a unique key (token_id + system timestamp in nanoseconds)
        // This ensures multiple purchases of the same token in the same period are tracked separately
        let trade_timestamp = std::time::Instant::now();
        let system_time_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let trade_key = format!("{}:{}", opportunity.selected_token_id, system_time_nanos);
        let mut pending = self.pending_trades.lock().await;
        
        let pending_trade = PendingTrade {
            token_id: opportunity.selected_token_id.clone(),
            condition_id: opportunity.selected_condition_id.clone(),
            investment_amount: fixed_amount,
            units,
            purchase_price: token_price,
            timestamp: trade_timestamp,
            market_timestamp,
            sold: false,
            eth_trend_token_id: opportunity.eth_higher_token_id.clone(),
            btc_trend_token_id: opportunity.btc_higher_token_id.clone(),
        };
        pending.insert(trade_key, pending_trade);
        drop(pending);
        
        let mut trades = self.trades_executed.lock().await;
        *trades += 1;
        let trades_count = *trades;
        drop(trades);

        // Calculate expected profit (if token wins, it's worth $1 per unit)
        // Expected profit = (payout per unit - purchase price) * units
        // If token wins: payout = $1.00, so profit = ($1.00 - purchase_price) * units
        let expected_profit_per_unit = 1.0 - token_price;
        let expected_profit = expected_profit_per_unit * units;
        let expected_profit_pct = (expected_profit_per_unit / token_price) * 100.0;
        
        // Get current total profit
        let total_profit = *self.total_profit.lock().await;

        info!("   ✅ Simulated Trade Executed - Investment: ${:.2} | Units: {:.2} | Expected Profit: ${:.4} ({:.2}%) | Total Profit: ${:.2} | Trades: {}",
              fixed_amount, units, expected_profit, expected_profit_pct, total_profit, trades_count);

        Ok(())
    }

    async fn execute_real_trend_trade(&self, opportunity: &TrendOpportunity, market_timestamp: u64) -> Result<()> {
        info!("🚀 PRODUCTION: Executing real trend trade...");
        
        let fixed_amount = self.config.fixed_trade_amount;
        let token_price = f64::try_from(opportunity.selected_token_price).unwrap_or(1.0);
        let units = fixed_amount / token_price;
        
        info!("   Strategy: {:?}", opportunity.strategy);
        info!("   ETH higher token: ${:.2}, BTC higher token: ${:.2}",
              opportunity.eth_higher_token_price, opportunity.btc_higher_token_price);
        info!("   Selected token: ${:.2} (condition: {})",
              token_price, &opportunity.selected_condition_id[..16]);
        info!("   Fixed trade amount: ${:.2}", fixed_amount);
        info!("   Units to purchase: {:.2} shares", units);

        // Polymarket requires minimum tokens per order
        const MIN_ORDER_SIZE: f64 = 1.5;
        
        if units < MIN_ORDER_SIZE {
            anyhow::bail!(
                "Order size too small. Quantity: {:.2}. Minimum required: {:.0} tokens. \
                Increase fixed_trade_amount in config.json to at least ${:.2}",
                units, MIN_ORDER_SIZE, MIN_ORDER_SIZE * token_price
            );
        }
        
        // Round to 2 decimal places (Polymarket requirement: maximum 2 decimal places)
        let units_rounded = (units * 100.0).round() / 100.0;
        
        info!("   Units (rounded to 2 decimals): {:.2}", units_rounded);

        // Use MARKET order (FOK) for immediate execution
        info!("   🚀 Using MARKET order (FOK) for immediate execution at current market price");
        
        // Execute single order
        match self.api.place_market_order(
            &opportunity.selected_token_id,
            units_rounded,
            "BUY",
            Some("FOK"),
        ).await {
            Ok(response) => {
                if response.message.as_ref().map(|m| m.contains("successfully")).unwrap_or(false) {
                    info!("✅ Order EXECUTED (FOK market order)");
                    if let Some(order_id) = &response.order_id {
                        info!("   Order ID: {}", order_id);
                    }
                    if let Some(msg) = &response.message {
                        info!("   {}", msg);
                    }
                } else {
                    anyhow::bail!("Order returned but may not have executed successfully: {:?}", response.message);
                }
            }
            Err(e) => {
                anyhow::bail!("Failed to execute order: {}", e);
            }
        }

        // Track the trade with a unique key (token_id + system timestamp in nanoseconds)
        // This ensures multiple purchases of the same token in the same period are tracked separately
        let trade_timestamp = std::time::Instant::now();
        let system_time_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let trade_key = format!("{}:{}", opportunity.selected_token_id, system_time_nanos);
        let mut pending = self.pending_trades.lock().await;
        
        let pending_trade = PendingTrade {
            token_id: opportunity.selected_token_id.clone(),
            condition_id: opportunity.selected_condition_id.clone(),
            investment_amount: fixed_amount,
            units: units_rounded,
            purchase_price: token_price,
            timestamp: trade_timestamp,
            market_timestamp,
            sold: false,
            eth_trend_token_id: opportunity.eth_higher_token_id.clone(),
            btc_trend_token_id: opportunity.btc_higher_token_id.clone(),
        };
        pending.insert(trade_key, pending_trade);
        drop(pending);
        
        let mut trades = self.trades_executed.lock().await;
        *trades += 1;
        let trades_count = *trades;
        drop(trades);

        // Calculate expected profit (if token wins, it's worth $1 per unit)
        // Expected profit = (payout per unit - purchase price) * units
        // If token wins: payout = $1.00, so profit = ($1.00 - purchase_price) * units
        let expected_profit_per_unit = 1.0 - token_price;
        let expected_profit = expected_profit_per_unit * units_rounded;
        let expected_profit_pct = (expected_profit_per_unit / token_price) * 100.0;
        
        // Get current total profit
        let total_profit = *self.total_profit.lock().await;

        info!(
            "✅ Real Trade Executed - Investment: ${:.2} | Units: {:.2} | Expected Profit: ${:.4} ({:.2}%) | Total Profit: ${:.2} | Trades: {}",
            fixed_amount, units_rounded, expected_profit, expected_profit_pct, total_profit, trades_count
        );

        Ok(())
    }

    /// Reset period (call when a new 15-minute period starts)
    /// Note: No cooldown to reset - duration analysis provides natural cooldown
    pub async fn reset_period(&self) {
        // No-op: duration analysis already provides natural cooldown between trades
    }

    /// Check emergency sell conditions for all pending trades
    /// For each pending trade, checks if the higher (trend) token in that trade's market dropped below threshold
    /// Checks if EITHER trending token (ETH higher token OR BTC higher token) dropped below emergency_sell_threshold
    /// If so, sells ALL pending trades because the trend is breaking
    /// After selling, checks after 5 seconds if still holding and retries if needed
    pub async fn check_emergency_sell(&self, eth_condition_id: &str, btc_condition_id: &str) -> Result<()> {
        // Get pending trades first
        let pending_trades: Vec<(String, PendingTrade)> = {
            let pending = self.pending_trades.lock().await;
            pending.iter()
                .filter(|(_, trade)| !trade.sold) // Only check unsold trades
                .map(|(key, trade)| (key.clone(), trade.clone()))
                .collect()
        };
        
        if pending_trades.is_empty() {
            return Ok(());
        }

        // Get market data for token name lookup
        let (eth_market, btc_market) = {
            let eth_market = self.api.get_market(eth_condition_id).await?;
            let btc_market = self.api.get_market(btc_condition_id).await?;
            (eth_market, btc_market)
        };

        // Check each trade's ORIGINAL trending tokens (not current higher tokens)
        // If EITHER original trending token dropped below threshold, sell ALL trades
        let mut should_sell_all = false;
        let mut triggered_tokens = Vec::new();
        
        // Check prices of original trending tokens for all trades
        // We need to check if ANY trade's original trending tokens dropped below threshold
        for (_, trade) in &pending_trades {
            // Fetch prices of the ORIGINAL trending tokens from when this trade was made
            let (eth_trend_price_result, btc_trend_price_result) = tokio::join!(
                self.api.get_price(&trade.eth_trend_token_id, "SELL"),
                self.api.get_price(&trade.btc_trend_token_id, "SELL"),
            );
            
            let eth_trend_price = eth_trend_price_result.ok()
                .and_then(|p| f64::try_from(p).ok());
            let btc_trend_price = btc_trend_price_result.ok()
                .and_then(|p| f64::try_from(p).ok());
            
            if let Some(eth_price) = eth_trend_price {
                if eth_price < self.config.emergency_sell_threshold {
                    should_sell_all = true;
                    triggered_tokens.push(format!("ETH trending token (${:.2})", eth_price));
                }
            }
            
            if let Some(btc_price) = btc_trend_price {
                if btc_price < self.config.emergency_sell_threshold {
                    should_sell_all = true;
                    triggered_tokens.push(format!("BTC trending token (${:.2})", btc_price));
                }
            }
        }
        
        if !should_sell_all {
            // All original trending tokens are still above threshold - no emergency sell needed
            return Ok(());
        }
        
        // One or more original trending tokens dropped below threshold - sell ALL pending trades
        info!("🚨 EMERGENCY SELL TRIGGERED: Original trending token(s) below threshold ${:.2}", 
              self.config.emergency_sell_threshold);
        for token_info in &triggered_tokens {
            info!("   {} dropped below threshold", token_info);
        }
        info!("   Selling ALL {} pending trade(s)...", pending_trades.len());
        
        // Collect all pending trades to sell
        let mut trades_to_sell = Vec::new();
        for (key, trade) in pending_trades {
            // Determine which market this trade belongs to
            let is_eth_market = trade.condition_id == eth_condition_id;
            let is_btc_market = trade.condition_id == btc_condition_id;
            
            // Determine token name for logging
            let market_name = if is_eth_market { "ETH" } else if is_btc_market { "BTC" } else { "Unknown" };
            
            let token_name = if is_eth_market {
                if let Some(token) = eth_market.tokens.iter().find(|t| t.token_id == trade.token_id) {
                    if token.outcome.to_uppercase().contains("UP") || token.outcome == "1" {
                        "ETH Up"
                    } else {
                        "ETH Down"
                    }
                } else {
                    "ETH Token"
                }
            } else if is_btc_market {
                if let Some(token) = btc_market.tokens.iter().find(|t| t.token_id == trade.token_id) {
                    if token.outcome.to_uppercase().contains("UP") || token.outcome == "1" {
                        "BTC Up"
                    } else {
                        "BTC Down"
                    }
                } else {
                    "BTC Token"
                }
            } else {
                "Unknown Token"
            };
            
            info!("   Selling trade {}: {} {} (condition: {})", 
                  &key[..16], market_name, token_name, &trade.condition_id[..16]);
            
            trades_to_sell.push((key, trade));
        }
        
        if trades_to_sell.is_empty() {
            return Ok(());
        }
        
        // Now sell all affected trades and buy opposite tokens
        let mut opposite_tokens_to_buy = Vec::new();
        
        for (key, trade) in &trades_to_sell {
            if let Err(e) = self.emergency_sell_trade(key, trade).await {
                warn!("Error executing emergency sell for trade {}: {}", key, e);
            }
            
            // Determine which original trending token triggered the sell
            // If ETH trending token dropped, buy ETH opposite token
            // If BTC trending token dropped, buy BTC opposite token
            let eth_trend_price_result = self.api.get_price(&trade.eth_trend_token_id, "SELL").await.ok()
                .and_then(|p| f64::try_from(p).ok());
            let btc_trend_price_result = self.api.get_price(&trade.btc_trend_token_id, "SELL").await.ok()
                .and_then(|p| f64::try_from(p).ok());
            
            let eth_triggered = eth_trend_price_result.map(|p| p < self.config.emergency_sell_threshold).unwrap_or(false);
            let btc_triggered = btc_trend_price_result.map(|p| p < self.config.emergency_sell_threshold).unwrap_or(false);
            
            // Buy opposite token for the market that triggered the sell
            if self.config.enable_opposite_side_buy {
                if eth_triggered && trade.condition_id == eth_condition_id {
                    // ETH trending token dropped - buy ETH opposite token
                    if let Ok(opposite_token_id) = self.get_opposite_token_id(&trade.condition_id, &trade.token_id).await {
                        opposite_tokens_to_buy.push((opposite_token_id, trade.condition_id.clone(), trade.eth_trend_token_id.clone()));
                    }
                } else if btc_triggered && trade.condition_id == btc_condition_id {
                    // BTC trending token dropped - buy BTC opposite token
                    if let Ok(opposite_token_id) = self.get_opposite_token_id(&trade.condition_id, &trade.token_id).await {
                        opposite_tokens_to_buy.push((opposite_token_id, trade.condition_id.clone(), trade.btc_trend_token_id.clone()));
                    }
                } else if eth_triggered {
                    // ETH trending token dropped but this is a BTC trade - still buy ETH opposite
                    if let Ok(opposite_token_id) = self.get_opposite_token_id(eth_condition_id, &trade.eth_trend_token_id).await {
                        opposite_tokens_to_buy.push((opposite_token_id, eth_condition_id.to_string(), trade.eth_trend_token_id.clone()));
                    }
                } else if btc_triggered {
                    // BTC trending token dropped but this is an ETH trade - still buy BTC opposite
                    if let Ok(opposite_token_id) = self.get_opposite_token_id(btc_condition_id, &trade.btc_trend_token_id).await {
                        opposite_tokens_to_buy.push((opposite_token_id, btc_condition_id.to_string(), trade.btc_trend_token_id.clone()));
                    }
                }
            }
        }
        
        // Buy opposite tokens (avoid duplicates)
        let mut seen_opposite = std::collections::HashSet::new();
        for (opposite_token_id, condition_id, original_trend_token_id) in opposite_tokens_to_buy {
            let key = format!("{}:{}", condition_id, opposite_token_id);
            if !seen_opposite.contains(&key) {
                seen_opposite.insert(key.clone());
                if let Err(e) = self.buy_opposite_token(&opposite_token_id, &condition_id, &original_trend_token_id).await {
                    warn!("Error buying opposite token {}: {}", &opposite_token_id[..16], e);
                }
            }
        }
        
        Ok(())
    }
    
    /// Get the opposite token ID for a given token in a market
    async fn get_opposite_token_id(&self, condition_id: &str, token_id: &str) -> Result<String> {
        let market = self.api.get_market(condition_id).await?;
        
        // Find the token that's opposite to the given token
        for token in &market.tokens {
            if token.token_id != token_id {
                // Check if this is the opposite token
                let outcome_upper = token.outcome.to_uppercase();
                let given_token = market.tokens.iter().find(|t| t.token_id == token_id);
                
                if let Some(given) = given_token {
                    let given_outcome_upper = given.outcome.to_uppercase();
                    // If given token is Up, return Down token, and vice versa
                    if (given_outcome_upper.contains("UP") || given_outcome_upper == "1") &&
                       (outcome_upper.contains("DOWN") || outcome_upper == "0") {
                        return Ok(token.token_id.clone());
                    } else if (given_outcome_upper.contains("DOWN") || given_outcome_upper == "0") &&
                              (outcome_upper.contains("UP") || outcome_upper == "1") {
                        return Ok(token.token_id.clone());
                    }
                }
            }
        }
        
        anyhow::bail!("Could not find opposite token for token {} in market {}", &token_id[..16], &condition_id[..16])
    }
    
    /// Buy opposite-side token when emergency sell triggers
    async fn buy_opposite_token(&self, opposite_token_id: &str, condition_id: &str, original_trend_token_id: &str) -> Result<()> {
        info!("🔄 Buying opposite-side token {} (original trend token: {})", 
              &opposite_token_id[..16], &original_trend_token_id[..16]);
        
        let fixed_amount = self.config.fixed_trade_amount;
        
        // Get current price
        let price_result = self.api.get_price(opposite_token_id, "BUY").await;
        let price = match price_result {
            Ok(p) => p,
            Err(e) => {
                warn!("⚠️  Could not fetch price for opposite token: {}", e);
                return Err(e);
            }
        };
        
        let price_f64 = f64::try_from(price).unwrap_or(0.0);
        let units = fixed_amount / price_f64;
        
        // Get current market timestamp
        let current_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let market_timestamp = (current_timestamp / 900) * 900;
        
        if self.simulation_mode {
            info!("   💰 SIMULATION: Buying {:.2} units of opposite token at ${:.4}", units, price_f64);
            info!("   📊 Investment: ${:.2} | Expected value if wins: ${:.2}", fixed_amount, units);
        } else {
            // Place real market order
            let units_rounded = (units * 10000.0).round() / 10000.0; // Round to 4 decimal places
            
            info!("   💰 PRODUCTION: Placing market order for {:.4} units at ${:.4}", units_rounded, price_f64);
            
            match self.api.place_market_order(opposite_token_id, units_rounded, "BUY", Some("FOK")).await {
                Ok(response) => {
                    info!("   ✅ Order placed: {:?}", response);
                }
                Err(e) => {
                    warn!("   ⚠️  Failed to place order: {}", e);
                    return Err(e);
                }
            }
        }
        
        // Track the opposite-side trade
        let trade_timestamp = std::time::Instant::now();
        let system_time_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let trade_key = format!("{}:{}", opposite_token_id, system_time_nanos);
        
        let opposite_trade = OppositeSideTrade {
            token_id: opposite_token_id.to_string(),
            condition_id: condition_id.to_string(),
            investment_amount: fixed_amount,
            units: if self.simulation_mode { units } else { units },
            purchase_price: price_f64,
            timestamp: trade_timestamp,
            market_timestamp,
            sold: false,
            original_trend_token_id: original_trend_token_id.to_string(),
        };
        
        let mut opposite_trades = self.opposite_side_trades.lock().await;
        opposite_trades.insert(trade_key.clone(), opposite_trade);
        drop(opposite_trades);
        
        info!("   ✅ Opposite-side trade tracked: {}", &trade_key[..16]);
        
        Ok(())
    }
    
    /// Check opposite-side trades and sell if profit/loss thresholds are met
    pub async fn check_opposite_side_trades(&self) -> Result<()> {
        let opposite_trades: Vec<(String, OppositeSideTrade)> = {
            let trades = self.opposite_side_trades.lock().await;
            trades.iter()
                .filter(|(_, trade)| !trade.sold)
                .map(|(key, trade)| (key.clone(), trade.clone()))
                .collect()
        };
        
        if opposite_trades.is_empty() {
            return Ok(());
        }
        
        for (key, trade) in opposite_trades {
            // Get current price
            let price_result = self.api.get_price(&trade.token_id, "SELL").await;
            let current_price = match price_result {
                Ok(p) => f64::try_from(p).unwrap_or(0.0),
                Err(_) => continue, // Skip if can't get price
            };
            
            // Calculate profit/loss percentage
            let price_change = current_price - trade.purchase_price;
            let price_change_pct = price_change / trade.purchase_price;
            
            // Check if profit threshold met (+50%)
            let profit_threshold = self.config.opposite_token_profit_threshold;
            let loss_threshold = -self.config.opposite_token_loss_threshold;
            
            let should_sell = if price_change_pct >= profit_threshold {
                info!("📈 Opposite token {} profit threshold met: {:.2}% (threshold: {:.2}%)", 
                      &trade.token_id[..16], price_change_pct * 100.0, profit_threshold * 100.0);
                true
            } else if price_change_pct <= loss_threshold {
                info!("📉 Opposite token {} loss threshold met: {:.2}% (threshold: {:.2}%)", 
                      &trade.token_id[..16], price_change_pct * 100.0, loss_threshold * 100.0);
                true
            } else {
                false
            };
            
            if should_sell {
                if let Err(e) = self.sell_opposite_token(&key, &trade, current_price).await {
                    warn!("Error selling opposite token {}: {}", key, e);
                }
            }
        }
        
        Ok(())
    }
    
    /// Sell an opposite-side token
    async fn sell_opposite_token(&self, trade_key: &str, trade: &OppositeSideTrade, current_price: f64) -> Result<()> {
        info!("💸 Selling opposite-side token {} at ${:.4}...", &trade.token_id[..16], current_price);
        
        if self.simulation_mode {
            let sell_value = current_price * trade.units;
            let profit = sell_value - trade.investment_amount;
            
            let mut total = self.total_profit.lock().await;
            *total += profit;
            let total_profit = *total;
            drop(total);
            
            // Mark as sold
            let mut opposite_trades = self.opposite_side_trades.lock().await;
            if let Some(t) = opposite_trades.get_mut(trade_key) {
                t.sold = true;
            }
            drop(opposite_trades);
            
            info!("   💰 Sold {:.2} units at ${:.4}", trade.units, current_price);
            info!("   📊 Profit: ${:.4} | Total Profit: ${:.2}", profit, total_profit);
        } else {
            // Place market sell order
            match self.api.place_market_order(&trade.token_id, trade.units, "SELL", Some("FOK")).await {
                Ok(response) => {
                    info!("   ✅ Sell order placed: {:?}", response);
                    
                    // Mark as sold
                    let mut opposite_trades = self.opposite_side_trades.lock().await;
                    if let Some(t) = opposite_trades.get_mut(trade_key) {
                        t.sold = true;
                    }
                    drop(opposite_trades);
                }
                Err(e) => {
                    warn!("   ⚠️  Failed to place sell order: {}", e);
                    return Err(e);
                }
            }
        }
        
        Ok(())
    }

    /// Emergency sell a specific trade
    /// After posting sell order, waits 5 seconds and checks if still holding, then retries if needed
    async fn emergency_sell_trade(&self, trade_key: &str, trade: &PendingTrade) -> Result<()> {
        info!("💸 Executing emergency sell for {:.2} units of token {}...", trade.units, &trade.token_id[..16]);
        
        if self.simulation_mode {
            // In simulation mode, calculate loss based on current price
            let price_result = self.api.get_price(&trade.token_id, "SELL").await;
            
            let price = match price_result {
                Ok(p) => p,
                Err(_) => {
                    warn!("⚠️  Could not fetch price for emergency sell calculation");
                    return Ok(());
                }
            };

            let price_f64 = f64::try_from(price).unwrap_or(0.0);
            let sell_value = price_f64 * trade.units;
            let loss = trade.investment_amount - sell_value;
            
            let mut total = self.total_profit.lock().await;
            *total -= loss; // Subtract loss from total profit
            let total_profit = *total;
            drop(total);
            
            // Mark as sold
            let mut pending = self.pending_trades.lock().await;
            if let Some(t) = pending.get_mut(trade_key) {
                t.sold = true;
            }
            drop(pending);
            
            info!("   💰 Emergency Sell - Sold {:.2} units at ${:.4}", trade.units, price_f64);
            info!("   📉 Loss: ${:.4} | Total Profit: ${:.2}", loss, total_profit);
        } else {
            // In production mode, use MARKET order for immediate execution
            info!("   🚨 Using MARKET order (FOK) for immediate execution");
            
            // Place market sell order
            match self.api.place_market_order(
                &trade.token_id,
                trade.units,
                "SELL",
                Some("FOK"), // Fill-or-Kill for immediate execution
            ).await {
                Ok(response) => {
                    info!("✅ Emergency sell order posted for {:.2} units", trade.units);
                    if let Some(order_id) = response.order_id {
                        info!("   Order ID: {}", order_id);
                    }
                    
                    // Wait 5 seconds, then check if still holding
                    tokio::time::sleep(Duration::from_secs(self.config.sell_retry_check_seconds)).await;
                    
                    // Check if we still have the token (by checking price - if we can't get price, assume sold)
                    let still_holding = match self.api.get_price(&trade.token_id, "SELL").await {
                        Ok(_) => {
                            // If we can get price, we might still be holding (or market still exists)
                            // For now, assume order executed successfully
                            false
                        }
                        Err(_) => {
                            // Can't get price - might mean we don't hold it anymore
                            false
                        }
                    };
                    
                    if still_holding {
                        warn!("⚠️  Still holding token after sell order, retrying...");
                        // Retry sell with current price
                        if let Err(e) = self.api.place_market_order(
                            &trade.token_id,
                            trade.units,
                            "SELL",
                            Some("FOK"),
                        ).await {
                            warn!("⚠️  Retry sell also failed: {}", e);
                        } else {
                            info!("✅ Retry sell order posted");
                        }
                    } else {
                        // Mark as sold
                        let mut pending = self.pending_trades.lock().await;
                        if let Some(t) = pending.get_mut(trade_key) {
                            t.sold = true;
                        }
                    }
                }
                Err(e) => {
                    warn!("⚠️  Failed to emergency sell token: {}", e);
                }
            }
        }
        
        Ok(())
    }

}

