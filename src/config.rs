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
    /// Minimum profit threshold for both tokens to allow trading
    /// Both tokens must have profit >= this value
    /// Default: 0.04 (4%)
    pub min_profit_threshold: f64,
    /// Fixed trade amount in USD for each purchase
    /// Default: 1.0 ($1.00)
    pub fixed_trade_amount: f64,
    /// Threshold for higher token price to trigger trend detection
    /// Both markets' higher tokens must be >= this price
    /// Default: 0.7 ($0.70)
    pub trend_detection_threshold: f64,
    /// Duration analysis period in seconds
    /// Bot analyzes data points for this duration after trend detection
    /// Default: 60 (1 minute)
    pub duration_analysis_seconds: u64,
    /// Minimum number of passed data points (out of total) to allow trading
    /// Total points = duration_analysis_seconds (assuming 1 point per second)
    /// Default: 40 (out of 60 points)
    pub min_passed_data_points: u64,
    /// Emergency sell threshold - sell token if price drops below this
    /// Default: 0.55 ($0.55)
    pub emergency_sell_threshold: f64,
    /// Time to wait after posting sell order before checking if still holding
    /// Default: 5 (5 seconds)
    pub sell_retry_check_seconds: u64,
    /// Interval for checking market closure after period ends
    /// Default: 20 (20 seconds)
    pub market_closure_check_interval_seconds: u64,
    /// Enable opposite-side token buy when emergency sell triggers
    /// Default: true
    pub enable_opposite_side_buy: bool,
    /// Profit threshold for opposite-side token (sell when price increases by this percentage)
    /// Default: 0.5 (50% profit)
    pub opposite_token_profit_threshold: f64,
    /// Loss threshold for opposite-side token (sell when price decreases by this percentage)
    /// Default: 0.1 (10% loss)
    pub opposite_token_loss_threshold: f64,
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
                min_profit_threshold: 0.04, // 4%
                fixed_trade_amount: 1.0, // $1.00
                trend_detection_threshold: 0.7, // $0.70
                duration_analysis_seconds: 60, // 1 minute
                min_passed_data_points: 40, // 40 out of 60 points
                emergency_sell_threshold: 0.55, // $0.55
                sell_retry_check_seconds: 5, // 5 seconds
                market_closure_check_interval_seconds: 20, // 20 seconds
                enable_opposite_side_buy: true, // Enable opposite-side buy
                opposite_token_profit_threshold: 0.5, // 50% profit
                opposite_token_loss_threshold: 0.1, // 10% loss
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

