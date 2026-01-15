use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    #[serde(rename = "conditionId")]
    pub condition_id: String,
    #[serde(rename = "id")]
    pub market_id: Option<String>, // Market ID (numeric string)
    pub question: String,
    pub slug: String,
    #[serde(rename = "resolutionSource")]
    pub resolution_source: Option<String>,
    #[serde(rename = "endDateISO")]
    pub end_date_iso: Option<String>,
    #[serde(rename = "endDateIso")]
    pub end_date_iso_alt: Option<String>,
    pub active: bool,
    pub closed: bool,
    pub tokens: Option<Vec<Token>>,
    #[serde(rename = "clobTokenIds")]
    pub clob_token_ids: Option<String>, // JSON string array
    pub outcomes: Option<String>, // JSON string array like "[\"Up\", \"Down\"]"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    #[serde(rename = "tokenId")]
    pub token_id: String,
    pub outcome: String,
    pub price: Option<Decimal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBook {
    pub bids: Vec<OrderBookEntry>,
    pub asks: Vec<OrderBookEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookEntry {
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Debug, Clone)]
pub struct TokenPrice {
    pub token_id: String,
    pub bid: Option<Decimal>,
    pub ask: Option<Decimal>,
}

impl TokenPrice {
    pub fn mid_price(&self) -> Option<Decimal> {
        match (self.bid, self.ask) {
            (Some(bid), Some(ask)) => Some((bid + ask) / Decimal::from(2)),
            (Some(bid), None) => Some(bid),
            (None, Some(ask)) => Some(ask),
            (None, None) => None,
        }
    }

    pub fn ask_price(&self) -> Decimal {
        self.ask.unwrap_or(Decimal::ZERO)
    }
}

/// Order structure for creating orders (before signing)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
    pub token_id: String,
    pub side: String, // "BUY" or "SELL"
    pub size: String,
    pub price: String,
    #[serde(rename = "type")]
    pub order_type: String, // "LIMIT" or "MARKET"
}

/// Signed order structure for posting to Polymarket
/// According to Polymarket docs, orders must be signed with private key before posting
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedOrder {
    // Order fields
    #[serde(rename = "tokenID")]
    pub token_id: String,
    pub side: String, // "BUY" or "SELL"
    pub size: String,
    pub price: String,
    #[serde(rename = "type")]
    pub order_type: String, // "LIMIT" or "MARKET"
    
    // Signature fields (will be populated when signing)
    pub signature: Option<String>,
    pub signer: Option<String>, // Address derived from private key
    pub nonce: Option<u64>,
    pub expiration: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderResponse {
    pub order_id: Option<String>,
    pub status: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceResponse {
    pub balance: String,
    pub allowance: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedeemResponse {
    pub success: bool,
    pub message: Option<String>,
    pub transaction_hash: Option<String>,
    pub amount_redeemed: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MarketData {
    pub condition_id: String,
    pub market_name: String,
    pub up_token: Option<TokenPrice>,
    pub down_token: Option<TokenPrice>,
}

#[derive(Debug, Clone)]
pub enum TrendStrategy {
    BothUp,    // Both markets trending up (ETH Up + BTC Up)
    BothDown,  // Both markets trending down (ETH Down + BTC Down)
}

#[derive(Debug, Clone)]
pub struct TrendOpportunity {
    pub strategy: TrendStrategy,
    pub eth_higher_token_price: Decimal,  // Price of ETH's higher token (Up or Down)
    pub btc_higher_token_price: Decimal,  // Price of BTC's higher token (Up or Down)
    pub eth_higher_token_id: String,      // Token ID for ETH's higher token
    pub btc_higher_token_id: String,      // Token ID for BTC's higher token
    pub eth_condition_id: String,
    pub btc_condition_id: String,
    pub selected_token_id: String,       // The token to buy (higher priced one)
    pub selected_token_price: Decimal,    // Price of the selected token
    pub selected_condition_id: String,    // Condition ID of the selected token's market
}

#[derive(Debug, Clone)]
pub struct PendingTrade {
    pub token_id: String,              // The token that was purchased
    pub condition_id: String,          // Condition ID of the market
    pub investment_amount: f64,        // Fixed trade amount (e.g., $1.00)
    pub units: f64,                    // Number of shares purchased
    pub purchase_price: f64,           // Price at which token was purchased
    pub timestamp: std::time::Instant, // When the trade was executed
    pub market_timestamp: u64,         // The 15-minute period timestamp when this trade was made (market closes at market_timestamp + 900 seconds)
    pub sold: bool,                    // Whether this token has been sold
    // Original trending tokens when trade was made (for emergency sell logic)
    pub eth_trend_token_id: String,    // Token ID of ETH's trending token (Up or Down) at trade time
    pub btc_trend_token_id: String,    // Token ID of BTC's trending token (Up or Down) at trade time
}

/// Opposite-side token trade (bought when emergency sell triggers)
#[derive(Debug, Clone)]
pub struct OppositeSideTrade {
    pub token_id: String,              // The opposite token that was purchased
    pub condition_id: String,          // Condition ID of the market
    pub investment_amount: f64,        // Fixed trade amount (e.g., $1.00)
    pub units: f64,                    // Number of shares purchased
    pub purchase_price: f64,           // Price at which token was purchased
    pub timestamp: std::time::Instant, // When the trade was executed
    pub market_timestamp: u64,         // The 15-minute period timestamp when this trade was made
    pub sold: bool,                    // Whether this token has been sold
    pub original_trend_token_id: String, // The original trending token that triggered the emergency sell
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketToken {
    pub outcome: String,
    pub price: rust_decimal::Decimal,
    #[serde(rename = "token_id")]
    pub token_id: String,
    pub winner: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketDetails {
    #[serde(rename = "accepting_order_timestamp")]
    pub accepting_order_timestamp: Option<String>,
    #[serde(rename = "accepting_orders")]
    pub accepting_orders: bool,
    pub active: bool,
    pub archived: bool,
    pub closed: bool,
    #[serde(rename = "condition_id")]
    pub condition_id: String,
    pub description: String,
    #[serde(rename = "enable_order_book")]
    pub enable_order_book: bool,
    #[serde(rename = "end_date_iso")]
    pub end_date_iso: String,
    pub fpmm: String,
    #[serde(rename = "game_start_time")]
    pub game_start_time: Option<String>,
    pub icon: String,
    pub image: String,
    #[serde(rename = "is_50_50_outcome")]
    pub is_50_50_outcome: bool,
    #[serde(rename = "maker_base_fee")]
    pub maker_base_fee: rust_decimal::Decimal,
    #[serde(rename = "market_slug")]
    pub market_slug: String,
    #[serde(rename = "minimum_order_size")]
    pub minimum_order_size: rust_decimal::Decimal,
    #[serde(rename = "minimum_tick_size")]
    pub minimum_tick_size: rust_decimal::Decimal,
    #[serde(rename = "neg_risk")]
    pub neg_risk: bool,
    #[serde(rename = "neg_risk_market_id")]
    pub neg_risk_market_id: String,
    #[serde(rename = "neg_risk_request_id")]
    pub neg_risk_request_id: String,
    #[serde(rename = "notifications_enabled")]
    pub notifications_enabled: bool,
    pub question: String,
    #[serde(rename = "question_id")]
    pub question_id: String,
    pub rewards: Rewards,
    #[serde(rename = "seconds_delay")]
    pub seconds_delay: u32,
    pub tags: Vec<String>,
    #[serde(rename = "taker_base_fee")]
    pub taker_base_fee: rust_decimal::Decimal,
    pub tokens: Vec<MarketToken>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rewards {
    #[serde(rename = "max_spread")]
    pub max_spread: rust_decimal::Decimal,
    #[serde(rename = "min_size")]
    pub min_size: rust_decimal::Decimal,
    pub rates: Option<serde_json::Value>,
}

