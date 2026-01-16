use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Run in simulation mode (no real trades)
    /// Default: simulation mode is enabled (true)
    #[arg(short, long, default_value_t = true)]
    pub simulation: bool,

    /// Run in production mode (execute real trades)
    /// This sets simulation to false
    #[arg(long)]
    pub no_simulation: bool,

    /// Configuration file path
    #[arg(short, long, default_value = "config.json")]
    pub config: PathBuf,
}

impl Args {
    /// Get the effective simulation mode
    /// If --no-simulation is used, it overrides the default
    pub fn is_simulation(&self) -> bool {
        if self.no_simulation {
            false
        } else {
            self.simulation
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub polymarket: PolymarketConfig,
    pub trading: TradingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolymarketConfig {
    pub gamma_api_url: String,
    pub clob_api_url: String,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub api_passphrase: Option<String>,
    /// Private key for signing orders (optional, but may be required for order placement)
    /// Format: hex string (with or without 0x prefix) or raw private key
    pub private_key: Option<String>,
    /// Proxy wallet address (Polymarket proxy wallet address where your balance is)
    /// If set, the bot will trade using this proxy wallet instead of the EOA (private key account)
    /// Format: Ethereum address (with or without 0x prefix)
    pub proxy_wallet_address: Option<String>,
    /// Signature type for authentication (optional, defaults to EOA if not set)
    /// 0 = EOA (Externally Owned Account - private key account)
    /// 1 = Proxy (Polymarket proxy wallet)
    /// 2 = GnosisSafe (Gnosis Safe wallet)
    /// If proxy_wallet_address is set, this should be 1 (Proxy)
    pub signature_type: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingConfig {
    pub eth_condition_id: Option<String>,
    pub btc_condition_id: Option<String>,
    pub check_interval_ms: u64,
    /// Fixed trade amount in USD for each purchase
    /// Default: 1.0 ($1.00)
    pub fixed_trade_amount: f64,
    /// Trigger price for ETH higher token ASK price (must hit this to trigger buy)
    /// Default: 0.99 ($0.99 ASK price - what we receive when selling)
    pub trigger_price: f64,
    /// Minimum time remaining in seconds (must be > this to buy)
    /// Default: 90 (90 seconds)
    pub min_time_remaining_seconds: u64,
    /// Interval for checking market closure after period ends
    /// Default: 20 (20 seconds)
    pub market_closure_check_interval_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            polymarket: PolymarketConfig {
                gamma_api_url: "https://gamma-api.polymarket.com".to_string(),
                clob_api_url: "https://clob.polymarket.com".to_string(),
                api_key: None,
                api_secret: None,
                api_passphrase: None,
                private_key: None,
                proxy_wallet_address: None,
                signature_type: None,
            },
            trading: TradingConfig {
                eth_condition_id: None,
                btc_condition_id: None,
                check_interval_ms: 1000,
                fixed_trade_amount: 1.0, // $1.00
                trigger_price: 0.99, // $0.99 ASK price
                min_time_remaining_seconds: 90, // 90 seconds
                market_closure_check_interval_seconds: 20, // 20 seconds
            },
        }
    }
}

impl Config {
    pub fn load(path: &PathBuf) -> anyhow::Result<Self> {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            Ok(serde_json::from_str(&content)?)
        } else {
            let config = Config::default();
            let content = serde_json::to_string_pretty(&config)?;
            std::fs::write(path, content)?;
            Ok(config)
        }
    }
}

