mod api;
mod detector;
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
use std::sync::{Mutex, OnceLock};

use api::PolymarketApi;
use detector::PriceDetector;
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

/// Global file writer for eprintln! messages to be saved to history.toml
static HISTORY_FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// Initialize the global history file writer
fn init_history_file(file: File) {
    HISTORY_FILE.set(Mutex::new(file)).expect("History file already initialized");
}

/// Write a message to both stderr and history.toml (without timestamp/level prefix)
pub fn log_to_history(message: &str) {
    // Write to stderr
    eprint!("{}", message);
    let _ = io::stderr().flush();
    
    // Write to history file
    if let Some(file_mutex) = HISTORY_FILE.get() {
        if let Ok(mut file) = file_mutex.lock() {
            let _ = write!(file, "{}", message);
            let _ = file.flush();
        }
    }
}

/// Macro to log to both stderr and history.toml (like eprintln! but also saves to file)
#[macro_export]
macro_rules! log_println {
    ($($arg:tt)*) => {
        {
            let message = format!($($arg)*);
            $crate::log_to_history(&format!("{}\n", message));
        }
    };
}

#[tokio::main]
async fn main() -> Result<()> {
    // Open log file in append mode
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("history.toml")
        .context("Failed to open history.toml for logging")?;
    
    // Initialize global history file for eprintln! messages
    init_history_file(log_file.try_clone().context("Failed to clone history file")?);
    
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

    eprintln!("🚀 Starting Polymarket Trend Trading Bot");
    eprintln!("📝 Logs are being saved to: history.toml");
    let is_simulation = args.is_simulation();
    eprintln!("Mode: {}", if is_simulation { "SIMULATION" } else { "PRODUCTION" });

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
        eprintln!("");
        eprintln!("═══════════════════════════════════════════════════════════");
        eprintln!("🔐 Authenticating with Polymarket CLOB API...");
        eprintln!("═══════════════════════════════════════════════════════════");
        
        match api.authenticate().await {
            Ok(_) => {
                eprintln!("✅ Authentication successful!");
                eprintln!("   Using private key and API credentials for signing");
                eprintln!("═══════════════════════════════════════════════════════════");
                eprintln!("");
            }
            Err(e) => {
                warn!("⚠️  Failed to authenticate: {}", e);
                warn!("⚠️  The bot will continue, but order placement may fail");
                warn!("⚠️  Please verify your credentials:");
                warn!("     1. private_key (hex string)");
                warn!("     2. api_key, api_secret, api_passphrase");
                eprintln!("");
            }
        }
    } else {
        eprintln!("💡 Simulation mode: Skipping authentication");
        eprintln!("");
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

    let detector = PriceDetector::new(
        config.trading.trigger_price,
        config.trading.min_time_remaining_seconds,
    );
    let trader = Trader::new(
        api.clone(),
        config.trading.clone(),
        is_simulation,
    );

    // Start monitoring
    let detector_arc = Arc::new(detector);
    let detector_clone = detector_arc.clone();
    let trader_arc = Arc::new(trader);
    let trader_clone = trader_arc.clone();
    
    // Start a background task to check pending trades and sell points
    let trader_check = trader_clone.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500)); // Check every 500ms for fast reaction
        loop {
            interval.tick().await;
            if let Err(e) = trader_check.check_pending_trades().await {
                warn!("Error checking pending trades: {}", e);
            }
        }
    });

    // Start a background task to check market closure
    let trader_closure = trader_clone.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(
            config.trading.market_closure_check_interval_seconds
        ));
        loop {
            interval.tick().await;
            if let Err(e) = trader_closure.check_market_closure().await {
                warn!("Error checking market closure: {}", e);
            }
        }
    });

    // Start a background task to detect new 15-minute periods and discover new markets
    let monitor_for_period_check = monitor_arc.clone();
    let api_for_period_check = api.clone();
    let trader_for_period_reset = trader_clone.clone();
    let detector_for_period_reset = detector_arc.clone();
    tokio::spawn(async move {
        loop {
            let current_market_timestamp = monitor_for_period_check.get_current_market_timestamp().await;
            let next_period_timestamp = current_market_timestamp + 900;
            
            let current_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            
            let sleep_duration = if next_period_timestamp > current_time {
                next_period_timestamp - current_time
            } else {
                0
            };
            
            eprintln!("⏰ Current market period: {}, next period starts in {} seconds", 
                current_market_timestamp, sleep_duration);
            
            tokio::time::sleep(tokio::time::Duration::from_secs(sleep_duration)).await;
            
            let current_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let current_period = (current_time / 900) * 900;
            
            eprintln!("🔄 New 15-minute period detected! (Period: {}) Discovering new markets...", current_period);
            
            let mut seen_ids = std::collections::HashSet::new();
            let (eth_id, btc_id) = monitor_for_period_check.get_current_condition_ids().await;
            seen_ids.insert(eth_id);
            seen_ids.insert(btc_id);
            
            match discover_market(&api_for_period_check, "ETH", "eth", current_time, &mut seen_ids).await {
                Ok(eth_market) => {
                    seen_ids.insert(eth_market.condition_id.clone());
                    match discover_market(&api_for_period_check, "BTC", "btc", current_time, &mut seen_ids).await {
                        Ok(btc_market) => {
                            if let Err(e) = monitor_for_period_check.update_markets(eth_market, btc_market).await {
                                warn!("Failed to update markets: {}", e);
                            } else {
                                trader_for_period_reset.reset_period(current_market_timestamp).await;
                                detector_for_period_reset.reset_period().await;
                            }
                        }
                        Err(e) => warn!("Failed to discover new BTC market: {}", e),
                    }
                }
                Err(e) => warn!("Failed to discover new ETH market: {}", e),
            }
        }
    });
    
    // Start monitoring with new detector
    monitor_arc.start_monitoring(move |snapshot| {
        let detector = detector_clone.clone();
        let trader = trader_clone.clone();
        
        async move {
            // Check for $0.98 trigger opportunity
            if let Some(opportunity) = detector.detect_opportunity(&snapshot).await {
                if let Err(e) = trader.execute_buy(&opportunity).await {
                    warn!("Error executing buy: {}", e);
                } else {
                    // Mark that we bought in this period
                    detector.mark_period_bought(opportunity.period_timestamp).await;
                }
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
            eprintln!("Found {} market by slug: {} | Condition ID: {}", market_name, market.slug, market.condition_id);
            return Ok(market);
        }
    }
    
    // Method 2: Try a few recent timestamps in case the current one doesn't exist yet
    for offset in 1..=3 {
        let try_time = rounded_time - (offset * 900); // Try previous 15-minute intervals
        let try_slug = format!("{}-updown-15m-{}", slug_prefix, try_time);
        eprintln!("Trying previous {} market by slug: {}", market_name, try_slug);
        if let Ok(market) = api.get_market_by_slug(&try_slug).await {
            if !seen_ids.contains(&market.condition_id) && market.active && !market.closed {
                eprintln!("Found {} market by slug: {} | Condition ID: {}", market_name, market.slug, market.condition_id);
                return Ok(market);
            }
        }
    }
    
    anyhow::bail!("Could not find active {} 15-minute up/down market. Please set condition_id in config.json", market_name)
}

