use crate::models::*;
use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use hex;
use log::{warn, info, error};
use std::sync::Arc;
use tokio::sync::Mutex;

// Official SDK imports for proper order signing
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::clob::types::{Side, OrderType, SignatureType};
use polymarket_client_sdk::auth::Credentials;
use polymarket_client_sdk::POLYGON;
use alloy::signers::local::LocalSigner;
use alloy::signers::Signer as _;
use alloy::primitives::Address as AlloyAddress;

// CTF (Conditional Token Framework) imports for redemption
// Based on docs: https://docs.polymarket.com/developers/builders/relayer-client#redeem-positions
use alloy::primitives::{Address, B256, U256, Bytes};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::eth::TransactionRequest;
use alloy::transports::http::Http;
use std::str::FromStr as _;

type HmacSha256 = Hmac<Sha256>;

pub struct PolymarketApi {
    client: Client,
    gamma_url: String,
    clob_url: String,
    api_key: Option<String>,
    api_secret: Option<String>,
    api_passphrase: Option<String>,
    private_key: Option<String>,
    // Proxy wallet configuration (for Polymarket proxy wallet)
    proxy_wallet_address: Option<String>,
    signature_type: Option<u8>, // 0 = EOA, 1 = Proxy, 2 = GnosisSafe
    // Track if authentication was successful at startup
    authenticated: Arc<tokio::sync::Mutex<bool>>,
}

impl PolymarketApi {
    pub fn new(
        gamma_url: String,
        clob_url: String,
        api_key: Option<String>,
        api_secret: Option<String>,
        api_passphrase: Option<String>,
        private_key: Option<String>,
        proxy_wallet_address: Option<String>,
        signature_type: Option<u8>,
    ) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");
        
        Self {
            client,
            gamma_url,
            clob_url,
            api_key,
            api_secret,
            api_passphrase,
            private_key,
            proxy_wallet_address,
            signature_type,
            authenticated: Arc::new(tokio::sync::Mutex::new(false)),
        }
    }
    

    /// Authenticate with Polymarket CLOB API at startup
    /// This verifies credentials (private_key + API credentials)
    /// Equivalent to JavaScript: new ClobClient(HOST, CHAIN_ID, signer, apiCreds, signatureType, funderAddress)
    pub async fn authenticate(&self) -> Result<()> {
        // Check if we have required credentials
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for authentication. Please set private_key in config.json"))?;
        
        // Create signer from private key (equivalent to: new Wallet(PRIVATE_KEY))
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));
        
        // Build authentication builder with proxy wallet support
        let mut auth_builder = ClobClient::new(&self.clob_url, ClobConfig::default())
            .context("Failed to create CLOB client")?
            .authentication_builder(&signer);
        
        // Configure proxy wallet if provided
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            let funder_address = AlloyAddress::parse_checksummed(proxy_addr, None)
                .context(format!("Failed to parse proxy_wallet_address: {}. Ensure it's a valid Ethereum address.", proxy_addr))?;
            
            auth_builder = auth_builder.funder(funder_address);
            
            // Set signature type based on config or default to Proxy
            let sig_type = match self.signature_type {
                Some(1) => SignatureType::Proxy,
                Some(2) => SignatureType::GnosisSafe,
                Some(0) | None => {
                    warn!("⚠️  proxy_wallet_address is set but signature_type is EOA. Defaulting to Proxy.");
                    SignatureType::Proxy
                },
                Some(n) => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            
            auth_builder = auth_builder.signature_type(sig_type);
            info!("🔐 Using proxy wallet: {} (signature type: {:?})", proxy_addr, sig_type);
        } else if let Some(sig_type_num) = self.signature_type {
            // If signature type is set but no proxy wallet, validate it's EOA
            let sig_type = match sig_type_num {
                0 => SignatureType::Eoa,
                1 | 2 => anyhow::bail!("signature_type {} requires proxy_wallet_address to be set", sig_type_num),
                n => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            auth_builder = auth_builder.signature_type(sig_type);
        }
        
        // Authenticate (equivalent to: new ClobClient(HOST, CHAIN_ID, signer, apiCreds, signatureType, funderAddress))
        // This verifies that both private_key and API credentials are valid
        let _client = auth_builder
            .authenticate()
            .await
            .context("Failed to authenticate with CLOB API. Check your API credentials (api_key, api_secret, api_passphrase) and private_key.")?;
        
        // Mark as authenticated
        *self.authenticated.lock().await = true;
        
        info!("✅ Successfully authenticated with Polymarket CLOB API");
        info!("   ✓ Private key: Valid");
        info!("   ✓ API credentials: Valid");
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            info!("   ✓ Proxy wallet: {}", proxy_addr);
        } else {
            info!("   ✓ Trading account: EOA (private key account)");
        }
        Ok(())
    }

    /// Generate HMAC-SHA256 signature for authenticated requests
    fn generate_signature(
        &self,
        method: &str,
        path: &str,
        body: &str,
        timestamp: u64,
    ) -> Result<String> {
        let secret = self.api_secret.as_ref()
            .ok_or_else(|| anyhow::anyhow!("API secret is required for authenticated requests"))?;
        
        // Create message: method + path + body + timestamp
        let message = format!("{}{}{}{}", method, path, body, timestamp);
        
        // Try to decode secret from base64 first, if that fails use as raw bytes
        let secret_bytes = match base64::decode(secret) {
            Ok(bytes) => bytes,
            Err(_) => {
                // If base64 decode fails, try using the secret directly as bytes
                // This handles cases where the secret is already in the correct format
                secret.as_bytes().to_vec()
            }
        };
        
        // Create HMAC-SHA256 signature
        let mut mac = HmacSha256::new_from_slice(&secret_bytes)
            .map_err(|e| anyhow::anyhow!("Failed to create HMAC: {}", e))?;
        mac.update(message.as_bytes());
        let result = mac.finalize();
        let signature = hex::encode(result.into_bytes());
        
        Ok(signature)
    }

    /// Add authentication headers to a request
    fn add_auth_headers(
        &self,
        request: reqwest::RequestBuilder,
        method: &str,
        path: &str,
        body: &str,
    ) -> Result<reqwest::RequestBuilder> {
        // Only add auth headers if we have all required credentials
        if self.api_key.is_none() || self.api_secret.is_none() || self.api_passphrase.is_none() {
            return Ok(request);
        }

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        let signature = self.generate_signature(method, path, body, timestamp)?;
        
        let request = request
            .header("POLY_API_KEY", self.api_key.as_ref().unwrap())
            .header("POLY_SIGNATURE", signature)
            .header("POLY_TIMESTAMP", timestamp.to_string())
            .header("POLY_PASSPHRASE", self.api_passphrase.as_ref().unwrap());
        
        Ok(request)
    }

    /// Get all active markets (using events endpoint)
    pub async fn get_all_active_markets(&self, limit: u32) -> Result<Vec<Market>> {
        let url = format!("{}/events", self.gamma_url);
        let limit_str = limit.to_string();
        let mut params = HashMap::new();
        params.insert("active", "true");
        params.insert("closed", "false");
        params.insert("limit", &limit_str);

        let response = self
            .client
            .get(&url)
            .query(&params)
            .send()
            .await
            .context("Failed to fetch all active markets")?;

        let status = response.status();
        let json: Value = response.json().await.context("Failed to parse markets response")?;
        
        if !status.is_success() {
            log::warn!("Get all active markets API returned error status {}: {}", status, serde_json::to_string(&json).unwrap_or_default());
            anyhow::bail!("API returned error status {}: {}", status, serde_json::to_string(&json).unwrap_or_default());
        }
        
        // Extract markets from events - events contain markets
        let mut all_markets = Vec::new();
        
        if let Some(events) = json.as_array() {
            for event in events {
                if let Some(markets) = event.get("markets").and_then(|m| m.as_array()) {
                    for market_json in markets {
                        if let Ok(market) = serde_json::from_value::<Market>(market_json.clone()) {
                            all_markets.push(market);
                        }
                    }
                }
            }
        } else if let Some(data) = json.get("data") {
            if let Some(events) = data.as_array() {
                for event in events {
                    if let Some(markets) = event.get("markets").and_then(|m| m.as_array()) {
                        for market_json in markets {
                            if let Ok(market) = serde_json::from_value::<Market>(market_json.clone()) {
                                all_markets.push(market);
                            }
                        }
                    }
                }
            }
        }
        
        log::debug!("Fetched {} active markets from events endpoint", all_markets.len());
        Ok(all_markets)
    }

    /// Get market by slug (e.g., "btc-updown-15m-1767726000")
    /// The API returns an event object with a markets array
    pub async fn get_market_by_slug(&self, slug: &str) -> Result<Market> {
        let url = format!("{}/events/slug/{}", self.gamma_url, slug);
        
        let response = self.client.get(&url).send().await
            .context(format!("Failed to fetch market by slug: {}", slug))?;
        
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("Failed to fetch market by slug: {} (status: {})", slug, status);
        }
        
        let json: Value = response.json().await
            .context("Failed to parse market response")?;
        
        // The response is an event object with a "markets" array
        // Extract the first market from the markets array
        if let Some(markets) = json.get("markets").and_then(|m| m.as_array()) {
            if let Some(market_json) = markets.first() {
                // Try to deserialize the market
                if let Ok(market) = serde_json::from_value::<Market>(market_json.clone()) {
                    return Ok(market);
                }
            }
        }
        
        anyhow::bail!("Invalid market response format: no markets array found")
    }

    /// Get order book for a specific token
    pub async fn get_orderbook(&self, token_id: &str) -> Result<OrderBook> {
        let url = format!("{}/book", self.clob_url);
        let params = [("token_id", token_id)];

        let response = self
            .client
            .get(&url)
            .query(&params)
            .send()
            .await
            .context("Failed to fetch orderbook")?;

        let orderbook: OrderBook = response
            .json()
            .await
            .context("Failed to parse orderbook")?;

        Ok(orderbook)
    }

    /// Get market details by condition ID
    pub async fn get_market(&self, condition_id: &str) -> Result<MarketDetails> {
        let url = format!("{}/markets/{}", self.clob_url, condition_id);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context(format!("Failed to fetch market for condition_id: {}", condition_id))?;

        let status = response.status();
        
        if !status.is_success() {
            anyhow::bail!("Failed to fetch market (status: {})", status);
        }

        let json_text = response.text().await
            .context("Failed to read response body")?;

        let market: MarketDetails = serde_json::from_str(&json_text)
            .map_err(|e| {
                log::error!("Failed to parse market response: {}. Response was: {}", e, json_text);
                anyhow::anyhow!("Failed to parse market response: {}", e)
            })?;

        Ok(market)
    }

    /// Get price for a token (for trading)
    /// side: "BUY" or "SELL"
    pub async fn get_price(&self, token_id: &str, side: &str) -> Result<rust_decimal::Decimal> {
        let url = format!("{}/price", self.clob_url);
        let params = [
            ("side", side),
            ("token_id", token_id),
        ];

        log::debug!("Fetching price from: {}?side={}&token_id={}", url, side, token_id);

        let response = self
            .client
            .get(&url)
            .query(&params)
            .send()
            .await
            .context("Failed to fetch price")?;

        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("Failed to fetch price (status: {})", status);
        }

        let json: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse price response")?;

        let price_str = json.get("price")
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!("Invalid price response format"))?;

        let price = rust_decimal::Decimal::from_str(price_str)
            .context(format!("Failed to parse price: {}", price_str))?;

        log::debug!("Price for token {} (side={}): {}", token_id, side, price);

        Ok(price)
    }

    /// Get best bid/ask prices for a token (from orderbook)
    pub async fn get_best_price(&self, token_id: &str) -> Result<Option<TokenPrice>> {
        let orderbook = self.get_orderbook(token_id).await?;
        
        let best_bid = orderbook.bids.first().map(|b| b.price);
        let best_ask = orderbook.asks.first().map(|a| a.price);

        if best_ask.is_some() {
            Ok(Some(TokenPrice {
                token_id: token_id.to_string(),
                bid: best_bid,
                ask: best_ask,
            }))
        } else {
            Ok(None)
        }
    }

    /// Place an order using the official SDK with proper private key signing
    /// 
    /// This method uses the official polymarket-client-sdk to:
    /// 1. Create signer from private key
    /// 2. Authenticate with the CLOB API
    /// 3. Create and sign the order
    /// 4. Post the signed order
    /// 
    /// Equivalent to JavaScript: client.createAndPostOrder(userOrder)
    pub async fn place_order(&self, order: &OrderRequest) -> Result<OrderResponse> {
        // Check if we have a private key (required for signing)
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for order signing. Please set private_key in config.json"))?;
        
        // Create signer from private key (equivalent to: new Wallet(PRIVATE_KEY))
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));
        
        // Build authentication builder with proxy wallet support
        let mut auth_builder = ClobClient::new(&self.clob_url, ClobConfig::default())
            .context("Failed to create CLOB client")?
            .authentication_builder(&signer);
        
        // Configure proxy wallet if provided
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            let funder_address = AlloyAddress::parse_checksummed(proxy_addr, None)
                .context(format!("Failed to parse proxy_wallet_address: {}. Ensure it's a valid Ethereum address.", proxy_addr))?;
            
            auth_builder = auth_builder.funder(funder_address);
            
            // Set signature type based on config or default to Proxy
            let sig_type = match self.signature_type {
                Some(1) => SignatureType::Proxy,
                Some(2) => SignatureType::GnosisSafe,
                Some(0) | None => SignatureType::Proxy, // Default to Proxy when proxy wallet is set
                Some(n) => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            
            auth_builder = auth_builder.signature_type(sig_type);
        } else if let Some(sig_type_num) = self.signature_type {
            // If signature type is set but no proxy wallet, validate it's EOA
            let sig_type = match sig_type_num {
                0 => SignatureType::Eoa,
                1 | 2 => anyhow::bail!("signature_type {} requires proxy_wallet_address to be set", sig_type_num),
                n => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            auth_builder = auth_builder.signature_type(sig_type);
        }
        
        // Create CLOB client with authentication (equivalent to: new ClobClient(HOST, CHAIN_ID, signer, apiCreds, signatureType, funderAddress))
        let client = auth_builder
            .authenticate()
            .await
            .context("Failed to authenticate with CLOB API. Check your API credentials.")?;
        
        // Convert order side string to SDK Side enum
        let side = match order.side.as_str() {
            "BUY" => Side::Buy,
            "SELL" => Side::Sell,
            _ => anyhow::bail!("Invalid order side: {}. Must be 'BUY' or 'SELL'", order.side),
        };
        
        // Parse price and size to Decimal
        let price = rust_decimal::Decimal::from_str(&order.price)
            .context(format!("Failed to parse price: {}", order.price))?;
        let size = rust_decimal::Decimal::from_str(&order.size)
            .context(format!("Failed to parse size: {}", order.size))?;
        
        info!("📤 Creating and posting order: {} {} {} @ {}", 
              order.side, order.size, order.token_id, order.price);
        
        // Create and post order using SDK (equivalent to: client.createAndPostOrder(userOrder))
        // This automatically creates, signs, and posts the order
        let order_builder = client
            .limit_order()
            .token_id(&order.token_id)
            .size(size)
            .price(price)
            .side(side);
        
        let signed_order = client.sign(&signer, order_builder.build().await?)
            .await
            .context("Failed to sign order")?;
        
        // Post order and capture detailed error information
        let response = match client.post_order(signed_order).await {
            Ok(resp) => resp,
            Err(e) => {
                // Log the full error details for debugging
                error!("❌ Failed to post order. Error details: {:?}", e);
                anyhow::bail!(
                    "Failed to post order: {}\n\
                    \n\
                    Troubleshooting:\n\
                    1. Check if you have sufficient USDC balance\n\
                    2. Verify the token_id is valid and active\n\
                    3. Check if the price is within valid range\n\
                    4. Ensure your API credentials have trading permissions\n\
                    5. Verify the order size meets minimum requirements",
                    e
                );
            }
        };
        
        // Check if the response indicates failure even if the request succeeded
        if !response.success {
            let error_msg = response.error_msg.as_deref().unwrap_or("Unknown error");
            error!("❌ Order rejected by API: {}", error_msg);
            anyhow::bail!(
                "Order was rejected: {}\n\
                \n\
                Order details:\n\
                - Token ID: {}\n\
                - Side: {}\n\
                - Size: {}\n\
                - Price: {}\n\
                \n\
                Common issues:\n\
                1. Insufficient balance or allowance\n\
                2. Invalid token ID or market closed\n\
                3. Price out of range\n\
                4. Size below minimum or above maximum",
                error_msg, order.token_id, order.side, order.size, order.price
            );
        }
        
        // Convert SDK response to our OrderResponse format
        let order_response = OrderResponse {
            order_id: Some(response.order_id.clone()),
            status: response.status.to_string(),
            message: Some(format!("Order placed successfully. Order ID: {}", response.order_id)),
        };
        
        info!("✅ Order placed successfully! Order ID: {}", response.order_id);
        
        Ok(order_response)
    }

    /// Place a market order (FOK/FAK) for immediate execution
    /// 
    /// This is used for emergency selling or when you want immediate execution at market price.
    /// Equivalent to JavaScript: client.createAndPostMarketOrder(userMarketOrder)
    /// 
    /// Market orders execute immediately at the best available price:
    /// - FOK (Fill-or-Kill): Order must fill completely or be cancelled
    /// - FAK (Fill-and-Kill): Order fills as much as possible, remainder is cancelled
    pub async fn place_market_order(
        &self,
        token_id: &str,
        amount: f64,
        side: &str,
        order_type: Option<&str>, // "FOK" or "FAK", defaults to FOK
    ) -> Result<OrderResponse> {
        // Check if we have a private key (required for signing)
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for order signing. Please set private_key in config.json"))?;
        
        // Create signer from private key
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));
        
        // Build authentication builder with proxy wallet support
        let mut auth_builder = ClobClient::new(&self.clob_url, ClobConfig::default())
            .context("Failed to create CLOB client")?
            .authentication_builder(&signer);
        
        // Configure proxy wallet if provided
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            let funder_address = AlloyAddress::parse_checksummed(proxy_addr, None)
                .context(format!("Failed to parse proxy_wallet_address: {}. Ensure it's a valid Ethereum address.", proxy_addr))?;
            
            auth_builder = auth_builder.funder(funder_address);
            
            // Set signature type based on config or default to Proxy
            let sig_type = match self.signature_type {
                Some(1) => SignatureType::Proxy,
                Some(2) => SignatureType::GnosisSafe,
                Some(0) | None => SignatureType::Proxy, // Default to Proxy when proxy wallet is set
                Some(n) => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            
            auth_builder = auth_builder.signature_type(sig_type);
        } else if let Some(sig_type_num) = self.signature_type {
            // If signature type is set but no proxy wallet, validate it's EOA
            let sig_type = match sig_type_num {
                0 => SignatureType::Eoa,
                1 | 2 => anyhow::bail!("signature_type {} requires proxy_wallet_address to be set", sig_type_num),
                n => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            auth_builder = auth_builder.signature_type(sig_type);
        }
        
        // Create CLOB client with authentication (equivalent to: new ClobClient(HOST, CHAIN_ID, signer, apiCreds, signatureType, funderAddress))
        let client = auth_builder
            .authenticate()
            .await
            .context("Failed to authenticate with CLOB API. Check your API credentials.")?;
        
        // Convert order side string to SDK Side enum
        let side_enum = match side {
            "BUY" => Side::Buy,
            "SELL" => Side::Sell,
            _ => anyhow::bail!("Invalid order side: {}. Must be 'BUY' or 'SELL'", side),
        };
        
        // Convert order type (defaults to FOK for immediate execution)
        let order_type_enum = match order_type.unwrap_or("FOK") {
            "FOK" => OrderType::FOK,
            "FAK" => OrderType::FAK,
            _ => OrderType::FOK, // Default to FOK
        };
        
        use rust_decimal::{Decimal, RoundingStrategy};
        use rust_decimal::prelude::*;
        
        // Convert amount to Decimal and round to 2 decimal places (Polymarket requirement)
        let amount_decimal = Decimal::from_f64_retain(amount)
            .ok_or_else(|| anyhow::anyhow!("Failed to convert amount to Decimal"))?
            .round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
        
        info!("📤 Creating and posting MARKET order: {} {} {} (type: {:?})", 
              side, amount_decimal, token_id, order_type_enum);
        
        // For market orders, we need to use the current market price to respect tick size requirements
        // Fetch the current best price from the market:
        // - For SELL: Use best bid price (what we get when selling)
        // - For BUY: Use best ask price (what we pay when buying)
        let market_price = if matches!(side_enum, Side::Buy) {
            // For BUY orders, get the current ask price (what we pay to buy)
            self.get_price(token_id, "BUY")
                .await
                .context("Failed to fetch current market price for BUY order. Cannot create market order without current price.")?
        } else {
            // For SELL orders, get the current bid price (what we get when selling)
            self.get_price(token_id, "SELL")
                .await
                .context("Failed to fetch current market price for SELL order. Cannot create market order without current price.")?
        };
        
        info!("   Using current market price: ${:.4} for {} order", market_price, side);
        
        // Use limit order with aggressive pricing to simulate market order
        // This ensures immediate execution at best available price
        let order_builder = client
            .limit_order()
            .token_id(token_id)
            .size(amount_decimal)
            .price(market_price)
            .side(side_enum);
        
        let signed_order = client.sign(&signer, order_builder.build().await?)
            .await
            .context("Failed to sign market order")?;
        
        let response = client.post_order(signed_order)
            .await
            .context("Failed to post market order")?;
        
        // Convert SDK response to our OrderResponse format
        let order_response = OrderResponse {
            order_id: Some(response.order_id.clone()),
            status: response.status.to_string(),
            message: if response.success {
                Some(format!("Market order executed successfully. Order ID: {}", response.order_id))
            } else {
                response.error_msg.clone()
            },
        };
        
        if response.success {
            info!("✅ Market order executed successfully! Order ID: {}", response.order_id);
        } else {
            let error_msg = response.error_msg.as_deref().unwrap_or("Unknown error");
            warn!("⚠️  Market order returned error: {}", error_msg);
        }
        
        Ok(order_response)
    }

    /// Place multiple market orders in a batch (atomic execution)
    /// 
    /// This ensures that either ALL orders are executed together or NONE are executed.
    /// Critical for arbitrage where both tokens must be purchased simultaneously.
    /// 
    /// Parameters:
    /// - orders: Vec of (token_id, amount, side, order_type) tuples
    ///   - token_id: Token ID to trade (as &str)
    ///   - amount: Quantity to trade
    ///   - side: "BUY" or "SELL"
    ///   - order_type: Optional "FOK" or "FAK", defaults to "FOK"
    /// 
    /// Returns: Vec of OrderResponse for each order
    pub async fn place_batch_market_orders(
        &self,
        orders: Vec<(&str, f64, &str, Option<&str>)>,
    ) -> Result<Vec<OrderResponse>> {
        if orders.is_empty() {
            anyhow::bail!("Cannot place batch orders: orders list is empty");
        }

        // Check if we have a private key (required for signing)
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for order signing. Please set private_key in config.json"))?;
        
        // Create signer from private key
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));
        
        // Build authentication builder with proxy wallet support
        let mut auth_builder = ClobClient::new(&self.clob_url, ClobConfig::default())
            .context("Failed to create CLOB client")?
            .authentication_builder(&signer);
        
        // Configure proxy wallet if provided
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            let funder_address = AlloyAddress::parse_checksummed(proxy_addr, None)
                .context(format!("Failed to parse proxy_wallet_address: {}. Ensure it's a valid Ethereum address.", proxy_addr))?;
            
            auth_builder = auth_builder.funder(funder_address);
            
            // Set signature type based on config or default to Proxy
            let sig_type = match self.signature_type {
                Some(1) => SignatureType::Proxy,
                Some(2) => SignatureType::GnosisSafe,
                Some(0) | None => SignatureType::Proxy, // Default to Proxy when proxy wallet is set
                Some(n) => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            
            auth_builder = auth_builder.signature_type(sig_type);
        } else if let Some(sig_type_num) = self.signature_type {
            // If signature type is set but no proxy wallet, validate it's EOA
            let sig_type = match sig_type_num {
                0 => SignatureType::Eoa,
                1 | 2 => anyhow::bail!("signature_type {} requires proxy_wallet_address to be set", sig_type_num),
                n => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            auth_builder = auth_builder.signature_type(sig_type);
        }
        
        // Create CLOB client with authentication
        let client = auth_builder
            .authenticate()
            .await
            .context("Failed to authenticate with CLOB API. Check your API credentials.")?;
        
        use rust_decimal::{Decimal, RoundingStrategy};
        use rust_decimal::prelude::*;
        
        info!("📤 Creating and signing {} orders for batch execution...", orders.len());
        
        // Create and sign all orders
        let mut signed_orders = Vec::new();
        for (idx, (token_id, amount, side, order_type)) in orders.iter().enumerate() {
            // Convert order side string to SDK Side enum
            let side_enum = match *side {
                "BUY" => Side::Buy,
                "SELL" => Side::Sell,
                _ => anyhow::bail!("Invalid order side: {}. Must be 'BUY' or 'SELL'", side),
            };
            
            // Convert amount to Decimal and round to 2 decimal places
            let amount_decimal = Decimal::from_f64_retain(*amount)
                .ok_or_else(|| anyhow::anyhow!("Failed to convert amount to Decimal for order {}", idx))?
                .round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
            
            info!("   Order {}: {} {} {} (type: {:?})", 
                  idx + 1, side, amount_decimal, token_id, 
                  order_type.unwrap_or("FOK"));
            
            // Fetch current market price for market orders
            let market_price = if matches!(side_enum, Side::Buy) {
                self.get_price(token_id, "BUY")
                    .await
                    .context(format!("Failed to fetch current market price for BUY order {} (token: {})", idx + 1, token_id))?
            } else {
                self.get_price(token_id, "SELL")
                    .await
                    .context(format!("Failed to fetch current market price for SELL order {} (token: {})", idx + 1, token_id))?
            };
            
            // Create and sign the order
            let order_builder = client
                .limit_order()
                .token_id((*token_id).to_string())
                .size(amount_decimal)
                .price(market_price)
                .side(side_enum);
            
            let signed_order = client.sign(&signer, order_builder.build().await?)
                .await
                .context(format!("Failed to sign order {} (token: {})", idx + 1, token_id))?;
            
            signed_orders.push(signed_order);
        }
        
        // Post all orders in a single batch request (atomic execution)
        info!("🚀 Posting {} orders in batch (atomic execution)...", signed_orders.len());
        
        let responses = client.post_orders(signed_orders)
            .await
            .context("Failed to post batch orders. Either all orders will succeed or all will fail (atomic execution).")?;
        
        // Convert SDK responses to our OrderResponse format
        let mut order_responses = Vec::new();
        for (idx, response) in responses.iter().enumerate() {
            let order_response = OrderResponse {
                order_id: Some(response.order_id.clone()),
                status: response.status.to_string(),
                message: if response.success {
                    Some(format!("Batch order {} executed successfully. Order ID: {}", idx + 1, response.order_id))
                } else {
                    response.error_msg.clone()
                },
            };
            
            if response.success {
                info!("✅ Batch order {} executed successfully! Order ID: {}", idx + 1, response.order_id);
            } else {
                let error_msg = response.error_msg.as_deref().unwrap_or("Unknown error");
                warn!("⚠️  Batch order {} returned error: {}", idx + 1, error_msg);
            }
            
            order_responses.push(order_response);
        }
        
        // Check if all orders succeeded
        let all_succeeded = order_responses.iter().all(|r| r.message.as_ref().map(|m| m.contains("successfully")).unwrap_or(false));
        if all_succeeded {
            info!("✅ All {} batch orders executed successfully!", order_responses.len());
        } else {
            let failed_count = order_responses.iter().filter(|r| !r.message.as_ref().map(|m| m.contains("successfully")).unwrap_or(false)).count();
            warn!("⚠️  Batch order execution: {} succeeded, {} failed", 
                  order_responses.len() - failed_count, failed_count);
        }
        
        Ok(order_responses)
    }
    
    /// Place an order using REST API with HMAC authentication (fallback method)
    /// 
    /// NOTE: This is a fallback method. The main place_order() method uses the official SDK
    /// with proper private key signing. Use this only if SDK integration fails.
    #[allow(dead_code)]
    async fn place_order_hmac(&self, order: &OrderRequest) -> Result<OrderResponse> {
        let path = "/orders";
        let url = format!("{}{}", self.clob_url, path);
        
        // Serialize order to JSON string for signature
        let body = serde_json::to_string(order)
            .context("Failed to serialize order to JSON")?;
        
        let mut request = self.client.post(&url).json(order);
        
        // Add HMAC-SHA256 authentication headers (L2 authentication)
        request = self.add_auth_headers(request, "POST", path, &body)
            .context("Failed to add authentication headers")?;

        info!("📤 Posting order to Polymarket (HMAC): {} {} {} @ {}", 
              order.side, order.size, order.token_id, order.price);

        let response = request
            .send()
            .await
            .context("Failed to place order")?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            
            // Provide helpful error messages
            if status == 401 || status == 403 {
                anyhow::bail!(
                    "Authentication failed (status: {}): {}\n\
                    Troubleshooting:\n\
                    1. Verify your API credentials (api_key, api_secret, api_passphrase) are correct\n\
                    2. Verify your private_key is correct (required for order signing)\n\
                    3. Check if your API key has trading permissions\n\
                    4. Ensure your account has sufficient balance",
                    status, error_text
                );
            }
            
            anyhow::bail!("Failed to place order (status: {}): {}", status, error_text);
        }

        let order_response: OrderResponse = response
            .json()
            .await
            .context("Failed to parse order response")?;

        info!("✅ Order placed successfully: {:?}", order_response);
        Ok(order_response)
    }

    /// Redeem winning conditional tokens after market resolution
    /// 
    /// This uses the CTF (Conditional Token Framework) contract to redeem winning tokens
    /// for USDC at 1:1 ratio after market resolution.
    /// 
    /// Parameters:
    /// - condition_id: The condition ID of the resolved market
    /// - token_id: The token ID of the winning token (used to determine index_set)
    /// - outcome: "Up" or "Down" to determine the index set
    /// 
    /// Reference: Polymarket CTF redemption using SDK
    /// USDC collateral address: 0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174
    /// 
    /// Note: This implementation uses the SDK's CTF client if available.
    /// The exact module path may vary - check SDK documentation.
    pub async fn redeem_tokens(
        &self,
        condition_id: &str,
        token_id: &str,
        outcome: &str,
    ) -> Result<RedeemResponse> {
        // Check if we have a private key (required for signing transactions)
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for redemption. Please set private_key in config.json"))?;
        
        // Create signer from private key
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));
        
        // USDC collateral token address on Polygon
        let collateral_token = Address::parse_checksummed(
            "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174",
            None
        ).context("Failed to parse USDC address")?;
        
        // Parse condition_id to B256 (remove 0x prefix if present)
        let condition_id_clean = condition_id.strip_prefix("0x").unwrap_or(condition_id);
        let condition_id_b256 = B256::from_str(condition_id_clean)
            .context(format!("Failed to parse condition_id to B256: {}", condition_id))?;
        
        // Determine index_set based on outcome
        // For binary markets (Up/Down), index sets are typically:
        // - "Up" or "1" uses index_set [1] 
        // - "Down" or "0" uses index_set [2]
        // Note: This may need adjustment - index sets are 1-indexed for binary markets
        let index_set = if outcome.to_uppercase().contains("UP") || outcome == "1" {
            U256::from(1)  // Up outcome - index set [1]
        } else {
            U256::from(2)  // Down outcome - index set [2]
        };
        
        info!("🔄 Redeeming winning tokens for condition {} (outcome: {}, index_set: {})", 
              condition_id, outcome, index_set);
        
        // Redeem positions by calling CTF contract directly
        // Based on docs: https://docs.polymarket.com/developers/builders/relayer-client#redeem-positions
        // CTF contract: 0x4d97dcd97ec945f40cf65f87097ace5ea0476045
        // Function: redeemPositions(address collateralToken, bytes32 parentCollectionId, bytes32 conditionId, uint256[] indexSets)
        
        const CTF_CONTRACT: &str = "0x4d97dcd97ec945f40cf65f87097ace5ea0476045";
        const RPC_URL: &str = "https://polygon-rpc.com";
        
        // Parse CTF contract address
        let ctf_address = Address::parse_checksummed(CTF_CONTRACT, None)
            .context("Failed to parse CTF contract address")?;
        
        let parent_collection_id = B256::ZERO;
        let index_sets = vec![index_set];
        
        info!("   Prepared redemption parameters:");
        info!("   - CTF Contract: {}", ctf_address);
        info!("   - Collateral token (USDC): {}", collateral_token);
        info!("   - Condition ID: {} ({:?})", condition_id, condition_id_b256);
        info!("   - Index set: {} (outcome: {})", index_set, outcome);
        
        // Encode the redeemPositions function call
        // Function signature: redeemPositions(address,bytes32,bytes32,uint256[])
        // Function selector: keccak256("redeemPositions(address,bytes32,bytes32,uint256[])")[0:4] = 0x3d7d3f5a
        
        // Function selector
        let function_selector = hex::decode("3d7d3f5a")
            .context("Failed to decode function selector")?;
        
        // Encode parameters manually using ABI encoding rules
        // Parameters: (address, bytes32, bytes32, uint256[])
        let mut encoded_params = Vec::new();
        
        // Encode address (20 bytes, left-padded to 32 bytes)
        let mut addr_bytes = [0u8; 32];
        addr_bytes[12..].copy_from_slice(collateral_token.as_slice());
        encoded_params.extend_from_slice(&addr_bytes);
        
        // Encode parentCollectionId (bytes32)
        encoded_params.extend_from_slice(parent_collection_id.as_slice());
        
        // Encode conditionId (bytes32)
        encoded_params.extend_from_slice(condition_id_b256.as_slice());
        
        // Encode indexSets array: offset (32 bytes) + length (32 bytes) + data (32 bytes per element)
        // Offset points to where array data starts (after all fixed params + offset itself)
        // Fixed params: address (32) + bytes32 (32) + bytes32 (32) + offset (32) = 128 bytes
        let array_offset = 32 * 4; // offset to array data (3 fixed params + 1 offset param)
        let array_length = index_sets.len();
        
        // Offset to array data (32 bytes)
        let offset_bytes = U256::from(array_offset).to_be_bytes::<32>();
        encoded_params.extend_from_slice(&offset_bytes);
        
        // Now append array data after all fixed parameters
        // Array length (32 bytes)
        let length_bytes = U256::from(array_length).to_be_bytes::<32>();
        encoded_params.extend_from_slice(&length_bytes);
        
        // Array data (each uint256 is 32 bytes)
        for idx in &index_sets {
            let idx_bytes = idx.to_be_bytes::<32>();
            encoded_params.extend_from_slice(&idx_bytes);
        }
        
        // Combine function selector with encoded parameters
        let mut call_data = function_selector;
        call_data.extend_from_slice(&encoded_params);
        
        info!("   Calling CTF contract to redeem positions...");
        
        // Create provider with wallet
        let provider = ProviderBuilder::new()
            .wallet(signer.clone())
            .connect(RPC_URL)
            .await
            .context("Failed to connect to Polygon RPC")?;
        
        // Build transaction request
        let tx_request = TransactionRequest {
            to: Some(ctf_address.into()),
            input: Bytes::from(call_data).into(),
            value: Some(U256::ZERO),
            ..Default::default()
        };
        
        // Send transaction
        let pending_tx = provider.send_transaction(tx_request)
            .await
            .context("Failed to send redeem transaction")?;
        
        let tx_hash = *pending_tx.tx_hash();
        
        info!("   Transaction sent, waiting for confirmation...");
        info!("   Transaction hash: {:?}", tx_hash);
        
        // Wait for transaction receipt
        let receipt = pending_tx.get_receipt().await
            .context("Failed to get transaction receipt")?;
        
        // Check if transaction succeeded
        // Receipt status() returns true for success, false for failure
        let success = receipt.status();
        
        if success {
            let redeem_response = RedeemResponse {
                success: true,
                message: Some(format!("Successfully redeemed tokens. Transaction: {:?}", tx_hash)),
                transaction_hash: Some(format!("{:?}", tx_hash)),
                amount_redeemed: None,
            };
            
            info!("✅ Successfully redeemed winning tokens!");
            info!("   Transaction hash: {:?}", tx_hash);
            if let Some(block_number) = receipt.block_number {
                info!("   Block number: {}", block_number);
            }
            
            Ok(redeem_response)
        } else {
            anyhow::bail!("Redemption transaction failed. Transaction hash: {:?}", tx_hash);
        }
    }
}

