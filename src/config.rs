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
    pub ws_url: String,
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
    pub min_profit_threshold: f64,
    pub max_position_size: f64,
    pub eth_condition_id: Option<String>,
    pub btc_condition_id: Option<String>,
    pub check_interval_ms: u64,
    pub trade_cooldown_seconds: u64,
    pub min_profit_improvement_pct: f64,
    pub emergency_sell_both_tokens_threshold: f64,
    pub emergency_sell_one_token_threshold: f64,
    pub emergency_sell_time_remaining_seconds: u64,
    /// Minimum time remaining (in seconds) before bot starts trading in a period
    /// Bot will only trade when time_remaining <= this value
    /// Default: 600 (10 minutes) - means bot waits 5 minutes before trading (15 min period - 10 min = 5 min wait)
    pub min_time_remaining_to_trade_seconds: u64,
    /// Time threshold (in seconds) for danger signal check
    /// When time remaining < this value, bot will check if both tokens are below danger_signal_min_token_price
    /// Default: 480 (8 minutes)
    pub danger_signal_time_threshold_seconds: u64,
    /// Minimum token price threshold for danger signal
    /// If time remaining < danger_signal_time_threshold_seconds AND both tokens < this price, reject trade
    /// Default: 0.45 ($0.45)
    pub danger_signal_min_token_price: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            polymarket: PolymarketConfig {
                gamma_api_url: "https://gamma-api.polymarket.com".to_string(),
                clob_api_url: "https://clob.polymarket.com".to_string(),
                ws_url: "wss://clob-ws.polymarket.com".to_string(),
                api_key: None,
                api_secret: None,
                api_passphrase: None,
                private_key: None,
                proxy_wallet_address: None,
                signature_type: None,
            },
            trading: TradingConfig {
                min_profit_threshold: 0.01,
                max_position_size: 100.0,
                eth_condition_id: None,
                btc_condition_id: None,
                check_interval_ms: 1000,
                trade_cooldown_seconds: 60,
                min_profit_improvement_pct: 0.20,
                emergency_sell_both_tokens_threshold: 0.3,
                emergency_sell_one_token_threshold: 0.1,
                emergency_sell_time_remaining_seconds: 120,
                min_time_remaining_to_trade_seconds: 600, // 10 minutes (wait 5 minutes before trading)
                danger_signal_time_threshold_seconds: 480, // 8 minutes
                danger_signal_min_token_price: 0.45, // $0.45
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

