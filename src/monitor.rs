use crate::api::PolymarketApi;
use crate::models::*;
use anyhow::Result;
use log::{debug, info, warn};
use std::sync::Arc;
use tokio::time::{sleep, Duration};

pub struct MarketMonitor {
    api: Arc<PolymarketApi>,
    eth_market: Arc<tokio::sync::Mutex<crate::models::Market>>,
    btc_market: Arc<tokio::sync::Mutex<crate::models::Market>>,
    check_interval: Duration,
    // Cached token IDs from getMarket() - refreshed once per period
    eth_up_token_id: Arc<tokio::sync::Mutex<Option<String>>>,
    eth_down_token_id: Arc<tokio::sync::Mutex<Option<String>>>,
    btc_up_token_id: Arc<tokio::sync::Mutex<Option<String>>>,
    btc_down_token_id: Arc<tokio::sync::Mutex<Option<String>>>,
    last_market_refresh: Arc<tokio::sync::Mutex<Option<std::time::Instant>>>,
    current_period_timestamp: Arc<tokio::sync::Mutex<u64>>, // Track current 15-minute period
}

#[derive(Debug, Clone)]
pub struct MarketSnapshot {
    pub eth_market: MarketData,
    pub btc_market: MarketData,
    pub timestamp: std::time::Instant,
    pub time_remaining_seconds: u64, // Time remaining in the current 15-minute period
    pub period_timestamp: u64, // The 15-minute period timestamp (e.g., 1767796200)
}

impl MarketMonitor {
    pub fn new(
        api: Arc<PolymarketApi>,
        eth_market: crate::models::Market,
        btc_market: crate::models::Market,
        check_interval_ms: u64,
    ) -> Self {
        // Calculate current 15-minute period timestamp
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let current_period = (current_time / 900) * 900; // Round to nearest 15 minutes
        
        Self {
            api,
            eth_market: Arc::new(tokio::sync::Mutex::new(eth_market)),
            btc_market: Arc::new(tokio::sync::Mutex::new(btc_market)),
            check_interval: Duration::from_millis(check_interval_ms),
            eth_up_token_id: Arc::new(tokio::sync::Mutex::new(None)),
            eth_down_token_id: Arc::new(tokio::sync::Mutex::new(None)),
            btc_up_token_id: Arc::new(tokio::sync::Mutex::new(None)),
            btc_down_token_id: Arc::new(tokio::sync::Mutex::new(None)),
            last_market_refresh: Arc::new(tokio::sync::Mutex::new(None)),
            current_period_timestamp: Arc::new(tokio::sync::Mutex::new(current_period)),
        }
    }

    /// Update markets when a new 15-minute period starts
    pub async fn update_markets(&self, eth_market: crate::models::Market, btc_market: crate::models::Market) -> Result<()> {
        info!("🔄 Updating to new 15-minute period markets...");
        info!("New ETH Market: {} ({})", eth_market.slug, eth_market.condition_id);
        info!("New BTC Market: {} ({})", btc_market.slug, btc_market.condition_id);
        
        *self.eth_market.lock().await = eth_market;
        *self.btc_market.lock().await = btc_market;
        
        // Reset token IDs - will be refreshed on next fetch
        *self.eth_up_token_id.lock().await = None;
        *self.eth_down_token_id.lock().await = None;
        *self.btc_up_token_id.lock().await = None;
        *self.btc_down_token_id.lock().await = None;
        *self.last_market_refresh.lock().await = None;
        
        // Update current period timestamp
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let new_period = (current_time / 900) * 900;
        *self.current_period_timestamp.lock().await = new_period;
        
        Ok(())
    }

    /// Check if we need to discover new markets (new 15-minute period started)
    pub async fn should_discover_new_markets(&self) -> bool {
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let current_period = (current_time / 900) * 900;
        
        let stored_period = *self.current_period_timestamp.lock().await;
        
        // If current period is different from stored period, we need new markets
        current_period != stored_period
    }

    /// Get current market condition IDs (for checking if markets are closed)
    pub async fn get_current_condition_ids(&self) -> (String, String) {
        let eth = self.eth_market.lock().await.condition_id.clone();
        let btc = self.btc_market.lock().await.condition_id.clone();
        (eth, btc)
    }

    /// Get the current market's timestamp from the ETH market slug
    pub async fn get_current_market_timestamp(&self) -> u64 {
        let eth_market = self.eth_market.lock().await;
        Self::extract_timestamp_from_slug(&eth_market.slug)
    }

    /// Refresh market data once per period (15 minutes) to get token IDs
    async fn refresh_market_tokens(&self) -> Result<()> {
        // Check if we need to refresh (every 15 minutes = 900 seconds)
        let should_refresh = {
            let last_refresh = self.last_market_refresh.lock().await;
            last_refresh
                .map(|last| last.elapsed().as_secs() >= 900)
                .unwrap_or(true)
        };

        if !should_refresh {
            return Ok(());
        }


        let (eth_condition_id, btc_condition_id) = self.get_current_condition_ids().await;

        // Get ETH market details
        if let Ok(eth_details) = self.api.get_market(&eth_condition_id).await {
            for token in &eth_details.tokens {
                let outcome_upper = token.outcome.to_uppercase();
                if outcome_upper.contains("UP") || outcome_upper == "1" {
                    *self.eth_up_token_id.lock().await = Some(token.token_id.clone());
                    info!("ETH Up token_id: {}", token.token_id);
                } else if outcome_upper.contains("DOWN") || outcome_upper == "0" {
                    *self.eth_down_token_id.lock().await = Some(token.token_id.clone());
                    info!("ETH Down token_id: {}", token.token_id);
                }
            }
        }

        // Get BTC market details
        if let Ok(btc_details) = self.api.get_market(&btc_condition_id).await {
            for token in &btc_details.tokens {
                let outcome_upper = token.outcome.to_uppercase();
                if outcome_upper.contains("UP") || outcome_upper == "1" {
                    *self.btc_up_token_id.lock().await = Some(token.token_id.clone());
                    info!("BTC Up token_id: {}", token.token_id);
                } else if outcome_upper.contains("DOWN") || outcome_upper == "0" {
                    *self.btc_down_token_id.lock().await = Some(token.token_id.clone());
                    info!("BTC Down token_id: {}", token.token_id);
                }
            }
        }

        *self.last_market_refresh.lock().await = Some(std::time::Instant::now());
        Ok(())
    }

    /// Fetch current market data for both ETH and BTC markets
    /// Uses get_price() endpoint continuously for real-time prices
    pub async fn fetch_market_data(&self) -> Result<MarketSnapshot> {
        // Refresh token IDs if needed (once per 15-minute period)
        self.refresh_market_tokens().await?;

        // Get market slugs to extract timestamps
        let eth_market_guard = self.eth_market.lock().await;
        let btc_market_guard = self.btc_market.lock().await;
        let eth_slug = eth_market_guard.slug.clone();
        let btc_slug = btc_market_guard.slug.clone();
        drop(eth_market_guard);
        drop(btc_market_guard);

        // Extract market timestamp from slug (e.g., "eth-updown-15m-1767796200" -> 1767796200)
        let eth_market_timestamp = Self::extract_timestamp_from_slug(&eth_slug);
        let btc_market_timestamp = Self::extract_timestamp_from_slug(&btc_slug);

        let (eth_condition_id, btc_condition_id) = self.get_current_condition_ids().await;
        
        // Fetch prices for all tokens using the price endpoint
        let eth_up_token_id = self.eth_up_token_id.lock().await.clone();
        let eth_down_token_id = self.eth_down_token_id.lock().await.clone();
        let btc_up_token_id = self.btc_up_token_id.lock().await.clone();
        let btc_down_token_id = self.btc_down_token_id.lock().await.clone();
        
        let (eth_up_price, eth_down_price, btc_up_price, btc_down_price) = tokio::join!(
            self.fetch_token_price(&eth_up_token_id, "ETH", "Up"),
            self.fetch_token_price(&eth_down_token_id, "ETH", "Down"),
            self.fetch_token_price(&btc_up_token_id, "BTC", "Up"),
            self.fetch_token_price(&btc_down_token_id, "BTC", "Down"),
        );

        // Get current timestamp
        let current_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Calculate remaining time for each market (15 minutes = 900 seconds per period)
        const PERIOD_DURATION: u64 = 900; // 15 minutes in seconds
        let eth_period_end = eth_market_timestamp + PERIOD_DURATION;
        let btc_period_end = btc_market_timestamp + PERIOD_DURATION;
        
        let eth_remaining_secs = if eth_period_end > current_timestamp {
            eth_period_end - current_timestamp
        } else {
            0
        };
        let btc_remaining_secs = if btc_period_end > current_timestamp {
            btc_period_end - current_timestamp
        } else {
            0
        };
        
        // Format remaining time as "Xm Ys"
        let format_remaining_time = |secs: u64| -> String {
            if secs == 0 {
                "0s".to_string()
            } else {
                let minutes = secs / 60;
                let seconds = secs % 60;
                if minutes > 0 {
                    format!("{}m {}s", minutes, seconds)
                } else {
                    format!("{}s", seconds)
                }
            }
        };
        
        let eth_remaining_str = format_remaining_time(eth_remaining_secs);
        let btc_remaining_str = format_remaining_time(btc_remaining_secs);

        // Log prices in the requested format
        let eth_up_str = eth_up_price.as_ref()
            .map(|p| format!("${:.2}", p.ask_price()))
            .unwrap_or_else(|| "N/A".to_string());
        let eth_down_str = eth_down_price.as_ref()
            .map(|p| format!("${:.2}", p.ask_price()))
            .unwrap_or_else(|| "N/A".to_string());
        let btc_up_str = btc_up_price.as_ref()
            .map(|p| format!("${:.2}", p.ask_price()))
            .unwrap_or_else(|| "N/A".to_string());
        let btc_down_str = btc_down_price.as_ref()
            .map(|p| format!("${:.2}", p.ask_price()))
            .unwrap_or_else(|| "N/A".to_string());

        info!(
            "ETH Up Token Price: {} Down Token Price: {} remaining time:{} market_timestamp:{}",
            eth_up_str, eth_down_str, eth_remaining_str, eth_market_timestamp
        );
        info!(
            "BTC Up Token Price: {} Down Token Price: {} remaining time:{} market_timestamp:{}",
            btc_up_str, btc_down_str, btc_remaining_str, btc_market_timestamp
        );
        info!(""); // Empty line for readability

        let eth_market_data = MarketData {
            condition_id: eth_condition_id,
            market_name: "ETH".to_string(),
            up_token: eth_up_price,
            down_token: eth_down_price,
        };

        let btc_market_data = MarketData {
            condition_id: btc_condition_id,
            market_name: "BTC".to_string(),
            up_token: btc_up_price,
            down_token: btc_down_price,
        };

        // Use ETH market's remaining time (both markets should have the same period)
        Ok(MarketSnapshot {
            eth_market: eth_market_data,
            btc_market: btc_market_data,
            timestamp: std::time::Instant::now(),
            time_remaining_seconds: eth_remaining_secs,
            period_timestamp: eth_market_timestamp, // Use ETH market timestamp as period identifier
        })
    }

    async fn fetch_token_price(
        &self,
        token_id: &Option<String>,
        market_name: &str,
        outcome: &str,
    ) -> Option<TokenPrice> {
        let token_id = token_id.as_ref()?;

        // Get BUY price (ask price - what we pay to buy)
        let buy_price = match self.api.get_price(token_id, "BUY").await {
            Ok(price) => Some(price),
            Err(e) => {
                warn!("Failed to fetch {} {} BUY price: {}", market_name, outcome, e);
                None
            }
        };

        // Get SELL price (bid price - what we get when selling)
        let sell_price = match self.api.get_price(token_id, "SELL").await {
            Ok(price) => Some(price),
            Err(e) => {
                warn!("Failed to fetch {} {} SELL price: {}", market_name, outcome, e);
                None
            }
        };

        if buy_price.is_some() || sell_price.is_some() {
            Some(TokenPrice {
                token_id: token_id.clone(),
                bid: sell_price,
                ask: buy_price,
            })
        } else {
            None
        }
    }

    /// Extract timestamp from market slug (e.g., "eth-updown-15m-1767796200" -> 1767796200)
    pub fn extract_timestamp_from_slug(slug: &str) -> u64 {
        // Slug format: {asset}-updown-15m-{timestamp}
        // Try to extract the timestamp (last number after the last dash)
        if let Some(last_dash) = slug.rfind('-') {
            if let Ok(timestamp) = slug[last_dash + 1..].parse::<u64>() {
                return timestamp;
            }
        }
        // Fallback: return 0 if we can't parse
        0
    }

    /// Start monitoring markets continuously
    /// Returns a callback function that can be used to update markets when new period starts
    pub async fn start_monitoring<F, Fut>(&self, callback: F)
    where
        F: Fn(MarketSnapshot) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        info!("Starting market monitoring...");
        
        loop {
            match self.fetch_market_data().await {
                Ok(snapshot) => {
                    debug!("Market snapshot updated");
                    callback(snapshot).await;
                }
                Err(e) => {
                    warn!("Error fetching market data: {}", e);
                }
            }
            
            sleep(self.check_interval).await;
        }
    }
}

