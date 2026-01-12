use crate::api::PolymarketApi;
use crate::models::*;
use crate::config::TradingConfig;
use anyhow::Result;
use log::{info, warn, debug};
use rust_decimal::Decimal;
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
    pending_trades: Arc<Mutex<HashMap<String, PendingTrade>>>, // Key: eth_condition_id + btc_condition_id
    market_cache: Arc<Mutex<HashMap<String, CachedMarketData>>>, // Key: condition_id, cache for 60 seconds
    last_trade_time: Arc<Mutex<Option<Instant>>>, // Track when we last executed a trade
    last_trade_profit_pct: Arc<Mutex<Option<f64>>>, // Track profit percentage of last trade
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
            market_cache: Arc::new(Mutex::new(HashMap::new())),
            last_trade_time: Arc::new(Mutex::new(None)),
            last_trade_profit_pct: Arc::new(Mutex::new(None)),
        }
    }

    /// Check and settle pending trades when markets close
    /// This is called periodically and also when a new period starts
    pub async fn check_pending_trades(&self) -> Result<()> {
        let mut pending = self.pending_trades.lock().await;
        let mut to_remove = Vec::new();
        
        // Only check trades that are at least 14 minutes old (markets close after 15 minutes)
        let min_age = Duration::from_secs(14 * 60);
        
        let pending_count = pending.len();
        if pending_count > 0 {
            debug!("Checking {} pending trades for market closure...", pending_count);
        }
        
        for (key, trade) in pending.iter() {
            let age = trade.timestamp.elapsed();
            
            // Skip checking if trade is too recent (markets won't be closed yet)
            if age < min_age {
                debug!("Trade {} is too recent (age: {:.1}s, need: {:.1}s), skipping", 
                       key, age.as_secs_f64(), min_age.as_secs_f64());
                continue;
            }
            
            info!("🔍 Checking market closure for trade {} (age: {:.1} minutes)", 
                  key, age.as_secs_f64() / 60.0);
            
            // Check if markets are closed (using cached data when possible)
            let (eth_closed, eth_winner) = self.check_market_result_cached(&trade.eth_condition_id, &trade.eth_token_id).await?;
            let (btc_closed, btc_winner) = self.check_market_result_cached(&trade.btc_condition_id, &trade.btc_token_id).await?;
            
            info!("   ETH Market ({}): closed={}, winner={}", 
                  &trade.eth_condition_id[..16], eth_closed, eth_winner);
            info!("   BTC Market ({}): closed={}, winner={}", 
                  &trade.btc_condition_id[..16], btc_closed, btc_winner);
            
            if eth_closed && btc_closed {
                // Both markets closed, redeem winning tokens and calculate actual profit
                if !self.simulation_mode {
                    // In production mode, redeem winning tokens (they're worth $1.00 USDC each)
                    // Note: Redeeming is different from selling - it's a direct conversion after resolution
                    self.redeem_winning_tokens(&trade, eth_winner, btc_winner).await;
                }
                
                let actual_profit = self.calculate_actual_profit(&trade, eth_winner, btc_winner);
                
                let mut total = self.total_profit.lock().await;
                *total += actual_profit;
                let total_profit = *total;
                drop(total);
                
                info!(
                    "💰 Market Closed - ETH Winner: {}, BTC Winner: {} | Actual Profit: ${:.4} | Total Profit: ${:.2}",
                    if eth_winner { "WON" } else { "LOST" },
                    if btc_winner { "WON" } else { "LOST" },
                    actual_profit,
                    total_profit
                );
                
                to_remove.push(key.clone());
            } else {
                info!("   ⏳ Markets not both closed yet (ETH: {}, BTC: {}), will check again...", 
                      eth_closed, btc_closed);
            }
        }
        
        for key in to_remove {
            pending.remove(&key);
        }
        
        Ok(())
    }

    /// Check previous period's markets when a new period starts
    /// This is called immediately when a new 15-minute period is detected
    /// It checks if the previous period's markets are closed and redeems if both are closed
    pub async fn check_previous_period_markets(&self, previous_eth_condition_id: &str, previous_btc_condition_id: &str) -> Result<()> {
        info!("🔍 Checking previous period's markets for closure...");
        info!("   Previous ETH Market: {}", &previous_eth_condition_id[..16]);
        info!("   Previous BTC Market: {}", &previous_btc_condition_id[..16]);
        
        // Find pending trades for the previous period
        let trade_key = format!("{}_{}", previous_eth_condition_id, previous_btc_condition_id);
        
        // Clone the trade data so we can drop the lock
        let trade_opt = {
            let pending = self.pending_trades.lock().await;
            pending.get(&trade_key).cloned()
        };
        
        if let Some(trade) = trade_opt {
            info!("   Found pending trade for previous period (age: {:.1} minutes)", 
                  trade.timestamp.elapsed().as_secs_f64() / 60.0);
            
            // Check if both markets are closed (force fresh check, don't use cache)
            let (eth_closed, eth_winner) = self.check_market_result(&trade.eth_condition_id, &trade.eth_token_id).await?;
            let (btc_closed, btc_winner) = self.check_market_result(&trade.btc_condition_id, &trade.btc_token_id).await?;
            
            info!("   Previous ETH Market: closed={}, winner={}", eth_closed, eth_winner);
            info!("   Previous BTC Market: closed={}, winner={}", btc_closed, btc_winner);
            
            if eth_closed && btc_closed {
                // Both markets closed, redeem immediately
                info!("✅ Both previous markets are closed! Redeeming winning tokens...");
                
                if !self.simulation_mode {
                    self.redeem_winning_tokens(&trade, eth_winner, btc_winner).await;
                }
                
                let actual_profit = self.calculate_actual_profit(&trade, eth_winner, btc_winner);
                
                let mut total = self.total_profit.lock().await;
                *total += actual_profit;
                let total_profit = *total;
                drop(total);
                
                info!(
                    "💰 Previous Period Closed - ETH Winner: {}, BTC Winner: {} | Actual Profit: ${:.4} | Total Profit: ${:.2}",
                    if eth_winner { "WON" } else { "LOST" },
                    if btc_winner { "WON" } else { "LOST" },
                    actual_profit,
                    total_profit
                );
                
                // Remove the trade from pending
                let mut pending = self.pending_trades.lock().await;
                pending.remove(&trade_key);
                drop(pending);
            } else {
                info!("   ⏳ Previous markets not both closed yet (ETH: {}, BTC: {}), will continue checking...", 
                      eth_closed, btc_closed);
            }
        } else {
            debug!("   No pending trades found for previous period");
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

    /// Redeem winning tokens when markets close (production mode only)
    /// 
    /// IMPORTANT: Redeeming is different from selling!
    /// - Selling: Before market resolves, at current market price
    /// - Redeeming: After market resolves, winning tokens redeemed for $1.00 USDC each
    /// 
    /// When markets close, winning tokens can be redeemed directly for USDC at 1:1 ratio.
    /// This is done through the CTF (Conditional Token Framework) redemption process.
    async fn redeem_winning_tokens(&self, trade: &PendingTrade, eth_winner: bool, btc_winner: bool) {
        // When markets close, winning tokens can be redeemed for $1.00 USDC each
        // This is different from selling - redemption is a direct conversion after resolution
        
        if eth_winner {
            // Determine outcome (Up or Down) by checking market data
            // Get market details to find which token is the winner
            let eth_outcome = match self.api.get_market(&trade.eth_condition_id).await {
                Ok(market_details) => {
                    // MarketDetails has tokens field which is Vec<MarketToken>
                    // Find the winning token and get its outcome
                    market_details.tokens
                        .iter()
                        .find(|t| t.token_id == trade.eth_token_id && t.winner)
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
            
            // Redeem ETH winning token
            match self.api.redeem_tokens(&trade.eth_condition_id, &trade.eth_token_id, &eth_outcome).await {
                Ok(response) => {
                    if response.success {
                        info!("✅ Redeemed {} units of ETH {} token (winner) for ${:.2} USDC", 
                              trade.units, eth_outcome, trade.units);
                        if let Some(tx_hash) = response.transaction_hash {
                            info!("   Transaction hash: {}", tx_hash);
                        }
                    } else {
                        warn!("⚠️  Redemption returned success=false: {:?}", response.message);
                    }
                }
                Err(e) => {
                    warn!("⚠️  Failed to redeem ETH {} token: {}", eth_outcome, e);
                    warn!("   Note: You may need to redeem manually through Polymarket UI");
                }
            }
        }
        
        if btc_winner {
            // Determine outcome (Up or Down) by checking market data
            let btc_outcome = match self.api.get_market(&trade.btc_condition_id).await {
                Ok(market_details) => {
                    // MarketDetails has tokens field which is Vec<MarketToken>
                    // Find the winning token and get its outcome
                    market_details.tokens
                        .iter()
                        .find(|t| t.token_id == trade.btc_token_id && t.winner)
                        .map(|t| t.outcome.clone())
                        .unwrap_or_else(|| {
                            // Fallback: assume "Down" if we can't find token
                            "Down".to_string()
                        })
                }
                Err(_) => {
                    // Fallback: assume "Down" if we can't fetch market
                    "Down".to_string()
                }
            };
            
            // Redeem BTC winning token
            match self.api.redeem_tokens(&trade.btc_condition_id, &trade.btc_token_id, &btc_outcome).await {
                Ok(response) => {
                    if response.success {
                        info!("✅ Redeemed {} units of BTC {} token (winner) for ${:.2} USDC", 
                              trade.units, btc_outcome, trade.units);
                        if let Some(tx_hash) = response.transaction_hash {
                            info!("   Transaction hash: {}", tx_hash);
                        }
                    } else {
                        warn!("⚠️  Redemption returned success=false: {:?}", response.message);
                    }
                }
                Err(e) => {
                    warn!("⚠️  Failed to redeem BTC {} token: {}", btc_outcome, e);
                    warn!("   Note: You may need to redeem manually through Polymarket UI");
                }
            }
        }
        
        if !eth_winner && !btc_winner {
            warn!("⚠️  Both tokens lost - nothing to redeem (both worth $0)");
        }
    }

    fn calculate_actual_profit(&self, trade: &PendingTrade, eth_winner: bool, btc_winner: bool) -> f64 {
        // We bought ETH Up + BTC Down
        // When markets close:
        // - If ETH Up wins: we get $1 per unit
        // - If BTC Down wins: we get $1 per unit
        // - If both win: we get $2 per unit
        // - If both lose: we get $0 per unit
        
        let payout_per_unit = if eth_winner && btc_winner {
            2.0 // Both won! (ETH went UP, BTC went DOWN)
        } else if eth_winner || btc_winner {
            1.0 // One won (break even or small profit)
        } else {
            0.0 // Both lost! (ETH went DOWN, BTC went UP) - TOTAL LOSS
        };
        
        let total_payout = payout_per_unit * trade.units;
        let actual_profit = total_payout - trade.investment_amount;
        
        if actual_profit < 0.0 {
            warn!("⚠️  LOSS: Both tokens lost! Lost ${:.4} on this trade", -actual_profit);
        }
        
        actual_profit
    }

    /// Execute arbitrage trade (with smart filtering to avoid too many trades)
    pub async fn execute_arbitrage(&self, opportunity: &ArbitrageOpportunity) -> Result<()> {
        // Check if we should execute this trade based on cooldown and profit improvement
        if !self.should_execute_trade(opportunity).await {
            debug!("⏸️  Skipping trade - cooldown active or insufficient profit improvement");
            return Ok(());
        }
        
        if self.simulation_mode {
            self.simulate_trade(opportunity).await
        } else {
            self.execute_real_trade(opportunity).await
        }
    }
    
    /// Determine if we should execute a trade based on:
    /// 1. Cooldown period (minimum time between trades) - checked FIRST
    /// 2. Profit improvement (new opportunity must be significantly better) - checked ONLY if cooldown passed
    async fn should_execute_trade(&self, opportunity: &ArbitrageOpportunity) -> bool {
        let min_cooldown_seconds = self.config.trade_cooldown_seconds;
        let min_profit_improvement_pct = self.config.min_profit_improvement_pct;
        
        let profit_pct = f64::try_from(opportunity.expected_profit / opportunity.total_cost * Decimal::from(100))
            .unwrap_or(0.0);
        
        let last_time = self.last_trade_time.lock().await;
        let last_profit_pct = self.last_trade_profit_pct.lock().await;
        
        // If we've never traded, allow it (no cooldown to check)
        if last_time.is_none() {
            drop(last_time);
            drop(last_profit_pct);
            return true;
        }
        
        // STEP 1: Check cooldown FIRST
        let time_since_last = last_time.unwrap().elapsed();
        let cooldown_passed = time_since_last.as_secs() >= min_cooldown_seconds;
        
        // If cooldown hasn't passed, reject immediately (don't check profit improvement)
        if !cooldown_passed {
            let remaining = min_cooldown_seconds - time_since_last.as_secs();
            drop(last_time);
            drop(last_profit_pct);
            debug!("⏸️  Trade rejected: Cooldown active ({} seconds remaining)", remaining);
            return false;
        }
        
        // STEP 2: Cooldown has passed, now check profit improvement
        if let Some(last_pct) = *last_profit_pct {
            let improvement = (profit_pct - last_pct) / last_pct.max(0.01);
            if improvement >= min_profit_improvement_pct {
                drop(last_time);
                drop(last_profit_pct);
                info!("✅ Trade approved: Cooldown passed + Profit improvement {:.1}% ({:.2}% -> {:.2}%)", 
                      improvement * 100.0, last_pct, profit_pct);
                return true;
            } else {
                drop(last_time);
                drop(last_profit_pct);
                debug!("⏸️  Trade rejected: Cooldown passed but profit improvement {:.1}% insufficient (need {:.0}%)", 
                       improvement * 100.0, min_profit_improvement_pct * 100.0);
                return false;
            }
        }
        
        // Cooldown passed and no previous profit data, allow trade
        drop(last_time);
        drop(last_profit_pct);
        true
    }

    async fn simulate_trade(&self, opportunity: &ArbitrageOpportunity) -> Result<()> {
        info!(
            "🔍 SIMULATION: Arbitrage opportunity detected!"
        );
        
        // Show correct labels based on strategy
        match opportunity.strategy {
            ArbitrageStrategy::EthUpBtcDown => {
                info!("   ETH Up Token Price: ${:.2}", opportunity.eth_token_price);
                info!("   BTC Down Token Price: ${:.2}", opportunity.btc_token_price);
            }
            ArbitrageStrategy::EthDownBtcUp => {
                info!("   ETH Down Token Price: ${:.2}", opportunity.eth_token_price);
                info!("   BTC Up Token Price: ${:.2}", opportunity.btc_token_price);
            }
        }
        
        info!(
            "   Total Cost: ${:.2}",
            opportunity.total_cost
        );
        info!(
            "   Expected Profit: ${:.2} ({:.2}%)",
            opportunity.expected_profit,
            (opportunity.expected_profit / opportunity.total_cost) * Decimal::from(100)
        );

        // Calculate position size (total dollar amount to invest)
        let position_size = self.calculate_position_size(opportunity);
        info!("   Position Size: ${:.2} (total investment amount)", position_size);
        
        // Calculate how many units we're buying
        let cost_per_unit = f64::try_from(opportunity.total_cost).unwrap_or(1.0);
        let units = position_size / cost_per_unit;

        // Show correct labels based on strategy
        match opportunity.strategy {
            ArbitrageStrategy::EthUpBtcDown => {
                info!("   ETH Up amount: ${:.2} ({} units × ${:.4})", 
                      units * f64::try_from(opportunity.eth_token_price).unwrap_or(0.0),
                      units, opportunity.eth_token_price);
                info!("   BTC Down amount: ${:.2} ({} units × ${:.4})", 
                      units * f64::try_from(opportunity.btc_token_price).unwrap_or(0.0),
                      units, opportunity.btc_token_price);
            }
            ArbitrageStrategy::EthDownBtcUp => {
                info!("   ETH Down amount: ${:.2} ({} units × ${:.4})", 
                      units * f64::try_from(opportunity.eth_token_price).unwrap_or(0.0),
                      units, opportunity.eth_token_price);
                info!("   BTC Up amount: ${:.2} ({} units × ${:.4})", 
                      units * f64::try_from(opportunity.btc_token_price).unwrap_or(0.0),
                      units, opportunity.btc_token_price);
            }
        }

        // In simulation mode, we track the trade and will calculate actual profit when markets close
        // Use condition IDs as key - accumulate multiple trades in the same period
        let trade_key = format!("{}_{}", opportunity.eth_condition_id, opportunity.btc_condition_id);
        
        let mut pending = self.pending_trades.lock().await;
        
        // If we already have a trade for this period, accumulate it (add units and investment)
        if let Some(existing_trade) = pending.get_mut(&trade_key) {
            // Accumulate: add new units and investment to existing trade
            existing_trade.units += units;
            existing_trade.investment_amount += position_size;
            info!("   📊 Accumulated trade: Total units: {:.2}, Total investment: ${:.2}", 
                  existing_trade.units, existing_trade.investment_amount);
        } else {
            // First trade for this period - create new entry
            let pending_trade = PendingTrade {
                eth_token_id: opportunity.eth_token_id.clone(),
                btc_token_id: opportunity.btc_token_id.clone(),
                eth_condition_id: opportunity.eth_condition_id.clone(),
                btc_condition_id: opportunity.btc_condition_id.clone(),
                investment_amount: position_size,
                units,
                timestamp: std::time::Instant::now(),
            };
            pending.insert(trade_key, pending_trade);
        }
        drop(pending);
        
        // Update last trade time and profit percentage
        let profit_pct = f64::try_from(opportunity.expected_profit / opportunity.total_cost * Decimal::from(100))
            .unwrap_or(0.0);
        *self.last_trade_time.lock().await = Some(Instant::now());
        *self.last_trade_profit_pct.lock().await = Some(profit_pct);
        
        let mut trades = self.trades_executed.lock().await;
        *trades += 1;
        let trades_count = *trades;
        drop(trades);

        info!(
            "   ✅ Simulated Trade Executed - Investment: ${:.2} | Expected Profit: ${:.2} ({:.2}%) | Trades: {}",
            position_size,
            f64::try_from(opportunity.expected_profit).unwrap_or(0.0) * units,
            profit_pct,
            trades_count
        );

        Ok(())
    }

    async fn execute_real_trade(&self, opportunity: &ArbitrageOpportunity) -> Result<()> {
        info!("🚀 PRODUCTION: Executing real arbitrage trade...");
        
        let position_size = self.calculate_position_size(opportunity);
        
        // Calculate token quantities from dollar investment
        // For arbitrage, we need to buy EQUAL NUMBER OF TOKENS for both tokens
        // because we need equal quantities to redeem (each token is worth $1 if it wins)
        // 
        // Calculation:
        // - Total cost per token pair = eth_price + btc_price
        // - Number of pairs we can buy = position_size / total_cost_per_pair
        // - This gives us equal quantities of both tokens
        let eth_token_price = f64::try_from(opportunity.eth_token_price).unwrap_or(1.0);
        let btc_token_price = f64::try_from(opportunity.btc_token_price).unwrap_or(1.0);
        let cost_per_unit = f64::try_from(opportunity.total_cost).unwrap_or(1.0);
        
        // Calculate how many units (pairs) we can buy
        // Each unit = 1 ETH token + 1 BTC token
        let units = position_size / cost_per_unit;
        
        // Both tokens have the same quantity (equal number of tokens)
        let eth_quantity = units;
        let btc_quantity = units;
        
        // Calculate actual dollar investment for each token
        let eth_investment = eth_quantity * eth_token_price;
        let btc_investment = btc_quantity * btc_token_price;
        
        info!("   Total Investment: ${:.2}", position_size);
        info!("   Cost per token pair: ${:.4} (ETH: ${:.4} + BTC: ${:.4})", 
              cost_per_unit, eth_token_price, btc_token_price);
        info!("   Token quantities: {} tokens each (ETH + BTC)", units);
        info!("   ETH investment: ${:.2} ({} tokens × ${:.4})", 
              eth_investment, eth_quantity, eth_token_price);
        info!("   BTC investment: ${:.2} ({} tokens × ${:.4})", 
              btc_investment, btc_quantity, btc_token_price);
        
        // Polymarket requires minimum tokens per order
        const MIN_ORDER_SIZE: f64 = 1.5;
        
        if eth_quantity < MIN_ORDER_SIZE || btc_quantity < MIN_ORDER_SIZE {
            anyhow::bail!(
                "Order size too small. ETH quantity: {:.2}, BTC quantity: {:.2}. Minimum required: {:.0} tokens per order. \
                Increase max_position_size in config.json to at least ${:.2}",
                eth_quantity, btc_quantity, MIN_ORDER_SIZE,
                MIN_ORDER_SIZE * cost_per_unit
            );
        }
        
        // Round to 2 decimal places (Polymarket requirement: maximum 2 decimal places)
        // Use proper rounding to avoid floating point precision issues
        let eth_quantity_rounded = (eth_quantity * 100.0).round() / 100.0;
        let btc_quantity_rounded = (btc_quantity * 100.0).round() / 100.0;
        
        info!("   Token quantities (rounded to 2 decimals): ETH: {:.2}, BTC: {:.2}", 
              eth_quantity_rounded, btc_quantity_rounded);

        // Use MARKET orders (FOK) for immediate execution
        // FOK (Fill-or-Kill): Order must fill completely or be cancelled
        // This ensures immediate execution at current market price, which is critical for arbitrage
        // where prices can change quickly and we need both orders to execute simultaneously
        info!("   🚀 Using MARKET orders (FOK) for immediate execution at current market price");
        
        // Execute both orders as market orders (FOK)
        let (eth_result, btc_result) = tokio::join!(
            self.api.place_market_order(
                &opportunity.eth_token_id,
                eth_quantity_rounded,
                "BUY",
                Some("FOK"), // Fill-or-Kill: must fill completely or be cancelled
            ),
            self.api.place_market_order(
                &opportunity.btc_token_id,
                btc_quantity_rounded,
                "BUY",
                Some("FOK"), // Fill-or-Kill: must fill completely or be cancelled
            )
        );

        // Check if both orders succeeded before logging
        let eth_success = eth_result.is_ok();
        let btc_success = btc_result.is_ok();
        
        // Log with correct labels based on strategy
        match opportunity.strategy {
            ArbitrageStrategy::EthUpBtcDown => {
                match &eth_result {
                    Ok(response) => {
                        info!("✅ ETH Up order EXECUTED (FOK market order): {:?}", response);
                        if let Some(order_id) = &response.order_id {
                            info!("   Order ID: {}", order_id);
                        }
                    }
                    Err(e) => {
                        warn!("❌ Failed to execute ETH Up order: {}", e);
                    }
                }
                match &btc_result {
                    Ok(response) => {
                        info!("✅ BTC Down order EXECUTED (FOK market order): {:?}", response);
                        if let Some(order_id) = &response.order_id {
                            info!("   Order ID: {}", order_id);
                        }
                    }
                    Err(e) => {
                        warn!("❌ Failed to execute BTC Down order: {}", e);
                    }
                }
            }
            ArbitrageStrategy::EthDownBtcUp => {
                match &eth_result {
                    Ok(response) => {
                        info!("✅ ETH Down order EXECUTED (FOK market order): {:?}", response);
                        if let Some(order_id) = &response.order_id {
                            info!("   Order ID: {}", order_id);
                        }
                    }
                    Err(e) => {
                        warn!("❌ Failed to execute ETH Down order: {}", e);
                    }
                }
                match &btc_result {
                    Ok(response) => {
                        info!("✅ BTC Up order EXECUTED (FOK market order): {:?}", response);
                        if let Some(order_id) = &response.order_id {
                            info!("   Order ID: {}", order_id);
                        }
                    }
                    Err(e) => {
                        warn!("❌ Failed to execute BTC Up order: {}", e);
                    }
                }
            }
        }
        
        // Check if both orders succeeded
        if !eth_success || !btc_success {
            anyhow::bail!(
                "Arbitrage trade failed: One or both market orders did not execute. \
                ETH order: {}, BTC order: {}",
                if eth_success { "SUCCESS" } else { "FAILED" },
                if btc_success { "SUCCESS" } else { "FAILED" }
            );
        }

        // Track the trade so we can sell tokens when markets close
        let cost_per_unit = f64::try_from(opportunity.total_cost).unwrap_or(1.0);
        let units = position_size / cost_per_unit;
        
        // Use condition IDs as key - accumulate multiple trades in the same period
        let trade_key = format!("{}_{}", opportunity.eth_condition_id, opportunity.btc_condition_id);
        
        let mut pending = self.pending_trades.lock().await;
        
        // If we already have a trade for this period, accumulate it (add units and investment)
        if let Some(existing_trade) = pending.get_mut(&trade_key) {
            // Accumulate: add new units and investment to existing trade
            existing_trade.units += units;
            existing_trade.investment_amount += position_size;
            info!("   📊 Accumulated trade: Total units: {:.2}, Total investment: ${:.2}", 
                  existing_trade.units, existing_trade.investment_amount);
        } else {
            // First trade for this period - create new entry
            let pending_trade = PendingTrade {
                eth_token_id: opportunity.eth_token_id.clone(),
                btc_token_id: opportunity.btc_token_id.clone(),
                eth_condition_id: opportunity.eth_condition_id.clone(),
                btc_condition_id: opportunity.btc_condition_id.clone(),
                investment_amount: position_size,
                units,
                timestamp: std::time::Instant::now(),
            };
            pending.insert(trade_key, pending_trade);
        }
        drop(pending);
        
        // Update last trade time and profit percentage
        let profit_pct = f64::try_from(opportunity.expected_profit / opportunity.total_cost * Decimal::from(100))
            .unwrap_or(0.0);
        *self.last_trade_time.lock().await = Some(Instant::now());
        *self.last_trade_profit_pct.lock().await = Some(profit_pct);
        
        let mut trades = self.trades_executed.lock().await;
        *trades += 1;
        let trades_count = *trades;
        drop(trades);

        info!(
            "✅ Real Trade Executed - Investment: ${:.2} | Expected Profit: ${:.4} ({:.2}%) | Trades: {}",
            position_size,
            f64::try_from(opportunity.expected_profit).unwrap_or(0.0) * units,
            profit_pct,
            trades_count
        );

        Ok(())
    }

    fn calculate_position_size(&self, opportunity: &ArbitrageOpportunity) -> f64 {
        // Position size is the total dollar amount to invest in this arbitrage opportunity
        // We use max_position_size from config as the maximum investment per trade
        let max_size = self.config.max_position_size;
        let cost_per_unit = f64::try_from(opportunity.total_cost).unwrap_or(1.0);
        
        // Calculate how many "units" (pairs of tokens) we can buy with max position size
        // Each unit costs total_cost (e.g., $0.75), so with $100 we can buy 100/0.75 = 133.33 units
        let units = max_size / cost_per_unit;
        
        // The actual position size is: units * cost_per_unit
        // But we cap it at max_size to not exceed our limit
        let position_size = (units * cost_per_unit).min(max_size);
        
        // For example:
        // - If total_cost = $0.75 and max_size = $100
        // - units = 100 / 0.75 = 133.33
        // - position_size = 133.33 * 0.75 = $100 (capped at max_size)
        // - This means we buy $100 worth of tokens total ($50 ETH Up + $50 BTC Down)
        position_size
    }

    /// Reset trade cooldown (call when a new 15-minute period starts)
    pub async fn reset_period(&self) {
        *self.last_trade_time.lock().await = None;
        *self.last_trade_profit_pct.lock().await = None;
        info!("🔄 Trade cooldown reset for new period");
    }

    /// Check clear condition sell: When 90s remain, sell losing token if outcome is clear
    /// Example: ETH Up = $0.96, BTC Down = $0.1 → sell BTC Down (it's clearly losing)
    pub async fn check_clear_condition_sell(&self, current_market_timestamp: u64) -> Result<()> {
        let pending = self.pending_trades.lock().await;
        let pending_count = pending.len();
        drop(pending);
        
        if pending_count == 0 {
            return Ok(());
        }

        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        // Calculate time remaining until market closes
        let market_end_timestamp = current_market_timestamp + 900;
        let time_remaining = if market_end_timestamp > current_time {
            market_end_timestamp - current_time
        } else {
            0
        };

        // Only check when configured time threshold or less remain
        if time_remaining > self.config.clear_condition_sell_time_threshold_seconds {
            return Ok(());
        }

        // Check all pending trades
        let trades_to_sell = {
            let pending = self.pending_trades.lock().await;
            let mut trades_to_sell = Vec::new();
            
            for (key, trade) in pending.iter() {
                // Fetch current prices for both tokens
                let eth_price_result = self.api.get_price(&trade.eth_token_id, "SELL").await;
                let btc_price_result = self.api.get_price(&trade.btc_token_id, "SELL").await;
                
                let (eth_price, btc_price) = match (eth_price_result, btc_price_result) {
                    (Ok(eth), Ok(btc)) => (eth, btc),
                    _ => {
                        debug!("⚠️  Could not fetch prices for clear condition check on trade {}", key);
                        continue;
                    }
                };

                let eth_price_f64 = f64::try_from(eth_price).unwrap_or(0.0);
                let btc_price_f64 = f64::try_from(btc_price).unwrap_or(0.0);

                // Clear condition: One token is very high (> $0.90) and the other is very low (< $0.20)
                // This indicates the outcome is clear - sell the losing token
                const HIGH_PRICE_THRESHOLD: f64 = 0.90;
                const LOW_PRICE_THRESHOLD: f64 = 0.20;
                
                let eth_is_winning = eth_price_f64 > HIGH_PRICE_THRESHOLD;
                let eth_is_losing = eth_price_f64 < LOW_PRICE_THRESHOLD;
                let btc_is_winning = btc_price_f64 > HIGH_PRICE_THRESHOLD;
                let btc_is_losing = btc_price_f64 < LOW_PRICE_THRESHOLD;

                // If one token is clearly winning and the other is clearly losing, sell the losing one
                if (eth_is_winning && btc_is_losing) || (btc_is_winning && eth_is_losing) {
                    let losing_token_id = if eth_is_losing {
                        trade.eth_token_id.clone()
                    } else {
                        trade.btc_token_id.clone()
                    };
                    let losing_token_price = if eth_is_losing { eth_price_f64 } else { btc_price_f64 };
                    let winning_token_price = if eth_is_winning { eth_price_f64 } else { btc_price_f64 };
                    
                    info!("🎯 CLEAR CONDITION DETECTED for trade {}:", key);
                    info!("   Time remaining: {} seconds (threshold: {} seconds)", 
                          time_remaining, self.config.clear_condition_sell_time_threshold_seconds);
                    info!("   ETH Token Price: ${:.2}", eth_price_f64);
                    info!("   BTC Token Price: ${:.2}", btc_price_f64);
                    info!("   Outcome is clear: {} token is winning (${:.2}), {} token is losing (${:.2})",
                          if eth_is_winning { "ETH" } else { "BTC" },
                          winning_token_price,
                          if eth_is_losing { "ETH" } else { "BTC" },
                          losing_token_price);
                    
                    trades_to_sell.push((key.clone(), trade.clone(), losing_token_id));
                }
            }
            
            trades_to_sell
        };
        
        // Sell the losing tokens
        for (key, trade, losing_token_id) in trades_to_sell {
            info!("💸 Selling losing token {} from trade {}", losing_token_id, key);
            
            if !self.simulation_mode {
                // In production, sell the losing token
                match self.api.place_market_order(
                    &losing_token_id,
                    trade.units,
                    "SELL",
                    Some("FOK"),
                ).await {
                    Ok(response) => {
                        info!("✅ Successfully sold losing token. Order ID: {:?}", response.order_id);
                        // Note: We don't remove the trade from pending because we still have the winning token
                        // The trade will be handled when the market closes
                    }
                    Err(e) => {
                        warn!("⚠️  Failed to sell losing token: {}", e);
                    }
                }
            } else {
                // In simulation, just log
                info!("   💰 SIMULATION: Would sell {} units of losing token", trade.units);
            }
        }
        
        Ok(())
    }

    /// Check emergency sell conditions for all pending trades
    /// Emergency sell triggers when ALL 3 conditions are met:
    /// 1. Both purchased tokens' prices are under emergency_sell_both_tokens_threshold
    /// 2. One token's price is under emergency_sell_one_token_threshold
    /// 3. Time remaining is less than emergency_sell_time_remaining_seconds
    pub async fn check_emergency_sell(&self, current_market_timestamp: u64) -> Result<()> {
        let pending = self.pending_trades.lock().await;
        let pending_count = pending.len();
        drop(pending);
        
        if pending_count == 0 {
            return Ok(());
        }

        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        // Calculate time remaining until market closes (market starts at timestamp, closes at timestamp + 900 seconds)
        let market_end_timestamp = current_market_timestamp + 900;
        let time_remaining = if market_end_timestamp > current_time {
            market_end_timestamp - current_time
        } else {
            0
        };

        let time_remaining_ok = time_remaining < self.config.emergency_sell_time_remaining_seconds;
        
        if !time_remaining_ok {
            // Time condition not met, skip checking prices
            return Ok(());
        }

        // Get current prices for all pending trades
        // First, collect trades that need emergency selling
        let trades_to_sell = {
            let pending = self.pending_trades.lock().await;
            let mut trades_to_sell = Vec::new();
            
            for (key, trade) in pending.iter() {
                // Fetch current prices for both tokens
                let eth_price_result = self.api.get_price(&trade.eth_token_id, "SELL").await;
                let btc_price_result = self.api.get_price(&trade.btc_token_id, "SELL").await;
                
                let (eth_price, btc_price) = match (eth_price_result, btc_price_result) {
                    (Ok(eth), Ok(btc)) => (eth, btc),
                    _ => {
                        debug!("⚠️  Could not fetch prices for emergency sell check on trade {}", key);
                        continue;
                    }
                };

                let eth_price_f64 = f64::try_from(eth_price).unwrap_or(0.0);
                let btc_price_f64 = f64::try_from(btc_price).unwrap_or(0.0);

                // Check condition 1: Both tokens under threshold
                let both_under_threshold = eth_price_f64 < self.config.emergency_sell_both_tokens_threshold
                    && btc_price_f64 < self.config.emergency_sell_both_tokens_threshold;
                
                // Check condition 2: One token under lower threshold
                let one_under_lower = eth_price_f64 < self.config.emergency_sell_one_token_threshold
                    || btc_price_f64 < self.config.emergency_sell_one_token_threshold;
                
                // All 3 conditions must be met
                if both_under_threshold && one_under_lower && time_remaining_ok {
                    info!("🚨 EMERGENCY SELL TRIGGERED for trade {}:", key);
                    info!("   ETH Token Price: ${:.2} (threshold: ${:.2})", 
                          eth_price_f64, self.config.emergency_sell_both_tokens_threshold);
                    info!("   BTC Token Price: ${:.2} (threshold: ${:.2})", 
                          btc_price_f64, self.config.emergency_sell_both_tokens_threshold);
                    info!("   One token under ${:.2}: {}", 
                          self.config.emergency_sell_one_token_threshold,
                          if eth_price_f64 < self.config.emergency_sell_one_token_threshold { "ETH" } else { "BTC" });
                    info!("   Time remaining: {} seconds (threshold: {} seconds)", 
                          time_remaining, self.config.emergency_sell_time_remaining_seconds);
                    
                    // Clone trade data for selling outside the lock
                    trades_to_sell.push((key.clone(), trade.clone()));
                }
            }
            
            trades_to_sell
        };
        
        // Now sell the trades (outside the lock)
        let mut to_remove = Vec::new();
        for (key, trade) in trades_to_sell {
            if let Err(e) = self.emergency_sell_trade(&trade).await {
                warn!("Error executing emergency sell for trade {}: {}", key, e);
            } else {
                to_remove.push(key);
            }
        }
        
        // Remove sold trades from pending
        if !to_remove.is_empty() {
            let mut pending = self.pending_trades.lock().await;
            for key in to_remove {
                pending.remove(&key);
            }
        }
        
        Ok(())
    }

    /// Emergency sell all holdings for a specific trade
    async fn emergency_sell_trade(&self, trade: &PendingTrade) -> Result<()> {
        info!("💸 Executing emergency sell for {} units...", trade.units);
        
        if self.simulation_mode {
            // In simulation mode, calculate loss based on current prices
            let eth_price_result = self.api.get_price(&trade.eth_token_id, "SELL").await;
            let btc_price_result = self.api.get_price(&trade.btc_token_id, "SELL").await;
            
            let (eth_price, btc_price) = match (eth_price_result, btc_price_result) {
                (Ok(eth), Ok(btc)) => (eth, btc),
                _ => {
                    warn!("⚠️  Could not fetch prices for emergency sell calculation");
                    return Ok(());
                }
            };

            let eth_price_f64 = f64::try_from(eth_price).unwrap_or(0.0);
            let btc_price_f64 = f64::try_from(btc_price).unwrap_or(0.0);
            
            let sell_value = (eth_price_f64 + btc_price_f64) * trade.units;
            let loss = trade.investment_amount - sell_value;
            
            let mut total = self.total_profit.lock().await;
            *total -= loss; // Subtract loss from total profit
            let total_profit = *total;
            drop(total);
            
            info!("   💰 Emergency Sell - Sold {} units at ETH: ${:.4}, BTC: ${:.4}", 
                  trade.units, eth_price_f64, btc_price_f64);
            info!("   📉 Loss: ${:.4} | Total Profit: ${:.2}", loss, total_profit);
        } else {
            // In production mode, use MARKET orders for immediate execution
            // Market orders execute at best available price immediately (FOK: Fill-or-Kill)
            info!("   🚨 Using MARKET orders for immediate execution");
            
            // Place market sell orders for both tokens
            // FOK (Fill-or-Kill) ensures immediate execution or cancellation
            let (eth_result, btc_result) = tokio::join!(
                self.api.place_market_order(
                    &trade.eth_token_id,
                    trade.units,
                    "SELL",
                    Some("FOK"), // Fill-or-Kill for immediate execution
                ),
                self.api.place_market_order(
                    &trade.btc_token_id,
                    trade.units,
                    "SELL",
                    Some("FOK"), // Fill-or-Kill for immediate execution
                )
            );
            
            match eth_result {
                Ok(response) => {
                    info!("✅ Emergency sold {} units of ETH token (market order)", trade.units);
                    if let Some(order_id) = response.order_id {
                        info!("   Order ID: {}", order_id);
                    }
                }
                Err(e) => warn!("⚠️  Failed to emergency sell ETH token: {}", e),
            }
            
            match btc_result {
                Ok(response) => {
                    info!("✅ Emergency sold {} units of BTC token (market order)", trade.units);
                    if let Some(order_id) = response.order_id {
                        info!("   Order ID: {}", order_id);
                    }
                }
                Err(e) => warn!("⚠️  Failed to emergency sell BTC token: {}", e),
            }
        }
        
        Ok(())
    }

    pub async fn get_stats(&self) -> (f64, u64) {
        let total = *self.total_profit.lock().await;
        let trades = *self.trades_executed.lock().await;
        (total, trades)
    }
}

