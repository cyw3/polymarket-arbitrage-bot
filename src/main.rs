mod api;
mod arbitrage;
mod config;
mod models;
mod monitor;
mod trader;

use anyhow::{Context, Result};
use clap::Parser;
use config::{Args, Config};
use log::{info, warn};
use std::sync::Arc;

use api::PolymarketApi;
use arbitrage::ArbitrageDetector;
use monitor::MarketMonitor;
use trader::Trader;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();

    let args = Args::parse();
    let config = Config::load(&args.config)?;

    info!("🚀 Starting Polymarket Arbitrage Bot");
    let is_simulation = args.is_simulation();
    info!("Mode: {}", if is_simulation { "SIMULATION" } else { "PRODUCTION" });

    // Initialize API client
    let api = Arc::new(PolymarketApi::new(
        config.polymarket.gamma_api_url.clone(),
        config.polymarket.clob_api_url.clone(),
        config.polymarket.api_key.clone(),
        config.polymarket.api_secret.clone(),
        config.polymarket.api_passphrase.clone(),
        config.polymarket.private_key.clone(),
    ));

    // Fetch and display account balance
    if !is_simulation {
        info!("");
        info!("═══════════════════════════════════════════════════════════");
        info!("📊 Fetching Polymarket Account Balance...");
        info!("═══════════════════════════════════════════════════════════");
        
        match api.get_balance().await {
            Ok(balance) => {
                use rust_decimal::Decimal;
                use std::str::FromStr;
                
                let balance_decimal = Decimal::from_str(&balance.balance)
                    .unwrap_or(Decimal::ZERO);
                let allowance_decimal = Decimal::from_str(&balance.allowance)
                    .unwrap_or(Decimal::ZERO);
                
                info!("💰 Proxy Wallet Balance: ${:.2} USDC", balance_decimal);
                info!("🔓 Token Allowance: ${:.2} USDC", allowance_decimal);
                
                let max_position_decimal = rust_decimal::Decimal::from_f64_retain(config.trading.max_position_size)
                    .unwrap_or(rust_decimal::Decimal::ZERO);
                
                if balance_decimal < max_position_decimal {
                    warn!("⚠️  WARNING: Account balance (${:.2}) is less than max_position_size (${:.2})", 
                          balance_decimal, config.trading.max_position_size);
                    warn!("⚠️  The bot may not be able to execute trades with current settings");
                } else {
                    let available_trades = (balance_decimal / max_position_decimal).floor();
                    info!("✅ Balance sufficient for up to {:.0} trades at max_position_size", available_trades);
                }
                
                info!("═══════════════════════════════════════════════════════════");
                info!("");
            }
            Err(e) => {
                warn!("⚠️  Failed to fetch account balance: {}", e);
                warn!("⚠️  Continuing anyway, but please verify your balance manually");
                warn!("⚠️  Make sure your API credentials are correct and have proper permissions");
                info!("");
            }
        }
    } else {
        info!("💡 Simulation mode: Skipping balance check");
        info!("");
    }

    // Get market data for ETH and BTC markets
    let (eth_market_data, btc_market_data) = 
        get_or_discover_markets(&api, &config).await?;

    // Initialize components
    let monitor = MarketMonitor::new(
        api.clone(),
        eth_market_data,
        btc_market_data,
        config.trading.check_interval_ms,
    );
    let monitor_arc = Arc::new(monitor);

    let detector = ArbitrageDetector::new(config.trading.min_profit_threshold);
    let trader = Trader::new(
        api.clone(),
        config.trading.clone(),
        is_simulation,
    );

    // Start monitoring
    let detector_clone = detector.clone();
    let trader_arc = Arc::new(trader);
    let trader_clone = trader_arc.clone();
    let monitor_for_trading = monitor_arc.clone();
    let api_for_discovery = api.clone();
    
    // Start a background task to check pending trades periodically
    // Check every 30 seconds to catch market closures quickly (markets close after 15 minutes)
    let trader_check = trader_clone.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30)); // Check every 30 seconds
        loop {
            interval.tick().await;
            if let Err(e) = trader_check.check_pending_trades().await {
                warn!("Error checking pending trades: {}", e);
            }
        }
    });

    // Start a background task to check emergency sell conditions periodically
    // Check every 10 seconds to catch emergency conditions quickly
    let trader_emergency = trader_clone.clone();
    let monitor_emergency = monitor_arc.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10)); // Check every 10 seconds
        loop {
            interval.tick().await;
            let current_market_timestamp = monitor_emergency.get_current_market_timestamp().await;
            if let Err(e) = trader_emergency.check_emergency_sell(current_market_timestamp).await {
                warn!("Error checking emergency sell conditions: {}", e);
            }
        }
    });

    // Start a background task to detect new 15-minute periods and discover new markets
    // Uses the current market's timestamp to calculate exactly when the next period starts
    let monitor_for_period_check = monitor_arc.clone();
    let api_for_period_check = api.clone();
    let trader_for_period_reset = trader_clone.clone();
    tokio::spawn(async move {
        loop {
            // Get current market's timestamp from slug (e.g., "eth-updown-15m-1767796200" -> 1767796200)
            let current_market_timestamp = monitor_for_period_check.get_current_market_timestamp().await;
            
            // Calculate when the next period starts: current_market_timestamp + 15 minutes (900 seconds)
            let next_period_timestamp = current_market_timestamp + 900;
            
            // Calculate how long to sleep until the next period
            let current_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            
            let sleep_duration = if next_period_timestamp > current_time {
                next_period_timestamp - current_time
            } else {
                // If we're already past the next period (shouldn't happen, but safety check)
                0
            };
            
            info!("⏰ Current market period: {}, next period starts in {} seconds (at {})", 
                current_market_timestamp, sleep_duration, next_period_timestamp);
            
            // Sleep until the next period starts
            tokio::time::sleep(tokio::time::Duration::from_secs(sleep_duration)).await;
            
            // Now discover new markets for the new period
            let current_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let current_period = (current_time / 900) * 900;
            
            info!("🔄 New 15-minute period detected! (Period: {}) Discovering new markets...", current_period);
            
            let mut seen_ids = std::collections::HashSet::new();
            // Get current condition IDs to avoid duplicates
            let (eth_id, btc_id) = monitor_for_period_check.get_current_condition_ids().await;
            seen_ids.insert(eth_id);
            seen_ids.insert(btc_id);
            
            // Discover new markets for current period
            match discover_market(&api_for_period_check, "ETH", "eth", current_time, &mut seen_ids).await {
                Ok(eth_market) => {
                    seen_ids.insert(eth_market.condition_id.clone());
                    match discover_market(&api_for_period_check, "BTC", "btc", current_time, &mut seen_ids).await {
                        Ok(btc_market) => {
                            if let Err(e) = monitor_for_period_check.update_markets(eth_market, btc_market).await {
                                warn!("Failed to update markets: {}", e);
                            } else {
                                // Reset trade cooldown for new period
                                trader_for_period_reset.reset_period().await;
                            }
                        }
                        Err(e) => warn!("Failed to discover new BTC market: {}", e),
                    }
                }
                Err(e) => warn!("Failed to discover new ETH market: {}", e),
            }
        }
    });
    
    monitor_arc.start_monitoring(move |snapshot| {
        let detector = detector_clone.clone();
        let trader = trader_clone.clone();
        
        async move {
            let opportunities = detector.detect_opportunities(&snapshot);
            
            for opportunity in opportunities {
                if let Err(e) = trader.execute_arbitrage(&opportunity).await {
                    warn!("Error executing trade: {}", e);
                }
            }
        }
    }).await;

    Ok(())
}

async fn get_or_discover_markets(
    api: &PolymarketApi,
    config: &Config,
) -> Result<(crate::models::Market, crate::models::Market)> {
    use crate::models::Market;
    
    let current_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    
    // Try multiple discovery methods - use a set to track seen IDs
    let mut seen_ids = std::collections::HashSet::new();
    
    // Use exact slug pattern: eth-updown-15m-{timestamp} and btc-updown-15m-{timestamp}
    let eth_market = discover_market(api, "ETH", "eth", current_time, &mut seen_ids).await
        .context("Failed to discover ETH market")?;
    seen_ids.insert(eth_market.condition_id.clone());
    
    let btc_market = discover_market(api, "BTC", "btc", current_time, &mut seen_ids).await
        .context("Failed to discover BTC market")?;

    if eth_market.condition_id == btc_market.condition_id {
        anyhow::bail!("ETH and BTC markets have the same condition ID: {}. This is incorrect. Please set condition IDs manually in config.json", eth_market.condition_id);
    }

    Ok((eth_market, btc_market))
}

async fn discover_market(
    api: &PolymarketApi,
    market_name: &str,
    slug_prefix: &str,
    current_time: u64,
    seen_ids: &mut std::collections::HashSet<String>,
) -> Result<crate::models::Market> {
    use crate::models::Market;
    
    // Method 1: Try to get by slug with current timestamp (rounded to nearest 15min)
    // Pattern: btc-updown-15m-{timestamp} or eth-updown-15m-{timestamp}
    let rounded_time = (current_time / 900) * 900; // Round to nearest 15 minutes
    let slug = format!("{}-updown-15m-{}", slug_prefix, rounded_time);
    
    if let Ok(market) = api.get_market_by_slug(&slug).await {

        if !seen_ids.contains(&market.condition_id) && market.active && !market.closed {
            log::info!("Found {} market by slug: {} | Condition ID: {}", market_name, market.slug, market.condition_id);
            return Ok(market);
        }
    }
    
    // Method 2: Try a few recent timestamps in case the current one doesn't exist yet
    for offset in 1..=3 {
        let try_time = rounded_time - (offset * 900); // Try previous 15-minute intervals
        let try_slug = format!("{}-updown-15m-{}", slug_prefix, try_time);
        log::info!("Trying previous {} market by slug: {}", market_name, try_slug);
        if let Ok(market) = api.get_market_by_slug(&try_slug).await {
            if !seen_ids.contains(&market.condition_id) && market.active && !market.closed {
                log::info!("Found {} market by slug: {} | Condition ID: {}", market_name, market.slug, market.condition_id);
                return Ok(market);
            }
        }
    }
    
    anyhow::bail!("Could not find active {} 15-minute up/down market. Please set condition_id in config.json", market_name)
}

