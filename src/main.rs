mod api;
mod trend;
mod config;
mod models;
mod monitor;
mod trader;

use anyhow::{Context, Result};
use clap::Parser;
use config::{Args, Config};
use log::{info, warn};
use std::sync::Arc;
use std::io::{self, Write};
use std::fs::{File, OpenOptions};
use std::sync::Mutex;

use api::PolymarketApi;
use trend::TrendDetector;
use monitor::MarketMonitor;
use trader::Trader;

/// A writer that writes to both stderr (terminal) and a file
/// Wrapped in Arc<Mutex<>> for thread-safe access
struct DualWriter {
    stderr: io::Stderr,
    file: Mutex<File>,
}

impl Write for DualWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Write to stderr (terminal) - stderr is already thread-safe
        let _ = self.stderr.write_all(buf);
        let _ = self.stderr.flush();
        
        // Write to file (protected by Mutex for thread safety)
        let mut file = self.file.lock().unwrap();
        file.write_all(buf)?;
        file.flush()?;
        
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stderr.flush()?;
        let mut file = self.file.lock().unwrap();
        file.flush()?;
        Ok(())
    }
}

// Make DualWriter Send + Sync for use with env_logger
unsafe impl Send for DualWriter {}
unsafe impl Sync for DualWriter {}

#[tokio::main]
async fn main() -> Result<()> {
    // Open log file in append mode
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("history.toml")
        .context("Failed to open history.toml for logging")?;
    
    // Create dual writer
    let dual_writer = DualWriter {
        stderr: io::stderr(),
        file: Mutex::new(log_file),
    };
    
    // Initialize logger with dual writer
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .target(env_logger::Target::Pipe(Box::new(dual_writer)))
        .init();

    let args = Args::parse();
    let config = Config::load(&args.config)?;

    info!("🚀 Starting Polymarket Trend Trading Bot");
    info!("📝 Logs are being saved to: history.toml");
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
        config.polymarket.proxy_wallet_address.clone(),
        config.polymarket.signature_type,
    ));

    // Authenticate with Polymarket CLOB API at startup
    // This verifies credentials and creates an authenticated client
    // Equivalent to JavaScript: new ClobClient(HOST, CHAIN_ID, signer, apiCreds)
    if !is_simulation {
        info!("");
        info!("═══════════════════════════════════════════════════════════");
        info!("🔐 Authenticating with Polymarket CLOB API...");
        info!("═══════════════════════════════════════════════════════════");
        
        match api.authenticate().await {
            Ok(_) => {
                info!("✅ Authentication successful!");
                info!("   Using private key and API credentials for signing");
                info!("═══════════════════════════════════════════════════════════");
                info!("");
            }
            Err(e) => {
                warn!("⚠️  Failed to authenticate: {}", e);
                warn!("⚠️  The bot will continue, but order placement may fail");
                warn!("⚠️  Please verify your credentials:");
                warn!("     1. private_key (hex string)");
                warn!("     2. api_key, api_secret, api_passphrase");
                info!("");
            }
        }
    } else {
        info!("💡 Simulation mode: Skipping authentication");
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

    let detector = TrendDetector::new(
        config.trading.trend_detection_threshold,
        config.trading.duration_analysis_seconds,
        config.trading.min_passed_data_points,
        config.trading.min_profit_threshold,
    );
    let trader = Trader::new(
        api.clone(),
        config.trading.clone(),
        is_simulation,
    );

    // Start monitoring
    let detector_clone = detector.clone();
    let trader_arc = Arc::new(trader);
    let trader_clone = trader_arc.clone();
    
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

    // Note: Emergency sell is now checked on every price update in the monitor callback
    // This provides faster response time (every 1 second instead of every 10 seconds)

    // Start a background task to detect new 15-minute periods and discover new markets
    // Uses the current market's timestamp to calculate exactly when the next period starts
    let monitor_for_period_check = monitor_arc.clone();
    let api_for_period_check = api.clone();
    let trader_for_period_reset = trader_clone.clone();
    let detector_for_period_reset = detector.clone();
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
            
            // Get previous period's condition IDs BEFORE updating to new markets
            let (previous_eth_id, previous_btc_id) = monitor_for_period_check.get_current_condition_ids().await;
            
            // Discover new markets for current period
            match discover_market(&api_for_period_check, "ETH", "eth", current_time, &mut seen_ids).await {
                Ok(eth_market) => {
                    seen_ids.insert(eth_market.condition_id.clone());
                    match discover_market(&api_for_period_check, "BTC", "btc", current_time, &mut seen_ids).await {
                        Ok(btc_market) => {
                            // Check previous period's markets BEFORE updating to new markets
                            info!("🔍 Checking if previous period's markets are closed...");
                            if let Err(e) = trader_for_period_reset.check_previous_period_markets(&previous_eth_id, &previous_btc_id).await {
                                warn!("Error checking previous period markets: {}", e);
                            }
                            
                            // Now update to new markets
                            if let Err(e) = monitor_for_period_check.update_markets(eth_market, btc_market).await {
                                warn!("Failed to update markets: {}", e);
                            } else {
                                // Reset period (no-op, but kept for consistency)
                                trader_for_period_reset.reset_period().await;
                                // Reset trend detector for new period
                                detector_for_period_reset.reset_period(current_period).await;
                            }
                        }
                        Err(e) => warn!("Failed to discover new BTC market: {}", e),
                    }
                }
                Err(e) => warn!("Failed to discover new ETH market: {}", e),
            }
        }
    });
    
    let monitor_for_emergency = monitor_arc.clone();
    monitor_arc.start_monitoring(move |snapshot| {
        let detector = detector_clone.clone();
        let trader = trader_clone.clone();
        let monitor_for_emergency = monitor_for_emergency.clone();
        
        async move {
            // Check for trend opportunities
            if let Some(opportunity) = detector.detect_opportunities(&snapshot).await {
                if let Err(e) = trader.execute_trend_trade(&opportunity, snapshot.period_timestamp).await {
                    warn!("Error executing trade: {}", e);
                }
            }
            
            // Check emergency sell conditions on every price update (faster response)
            let (eth_condition_id, btc_condition_id) = monitor_for_emergency.get_current_condition_ids().await;
            if let Err(e) = trader.check_emergency_sell(&eth_condition_id, &btc_condition_id).await {
                warn!("Error checking emergency sell conditions: {}", e);
            }
            
            // Check opposite-side trades for profit/loss thresholds
            if let Err(e) = trader.check_opposite_side_trades().await {
                warn!("Error checking opposite-side trades: {}", e);
            }
        }
    }).await;

    Ok(())
}

async fn get_or_discover_markets(
    api: &PolymarketApi,
    _config: &Config,
) -> Result<(crate::models::Market, crate::models::Market)> {
    
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

