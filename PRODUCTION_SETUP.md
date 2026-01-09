# Production Mode Setup Guide

This guide explains how to configure the bot for **real trading** (production mode) on Polymarket.

## Prerequisites

1. A Polymarket account with sufficient balance for trading
2. Access to Polymarket's Builder Profile to generate API credentials

## Step 1: Get API Credentials

1. **Navigate to Builder Profile:**
   - Go to [Polymarket Settings](https://polymarket.com/settings?tab=builder)
   - Or: Account Settings → "Builders" tab

2. **Create API Key:**
   - In the "Builder Keys" section, click **"+ Create New"**
   - You will receive three credentials:
     - **`apiKey`**: Your public API key identifier
     - **`secret`**: Secret key for signing requests (base64 encoded)
     - **`passphrase`**: Additional authentication passphrase

3. **Store Credentials Securely:**
   - ⚠️ **IMPORTANT**: Never commit these credentials to version control
   - Store them in environment variables or a secure config file
   - Keep them private and never share them

## Step 2: Configure the Bot

You have two options for providing credentials:

### Option A: Environment Variables (Recommended)

Set these environment variables before running the bot:

```bash
export POLYMARKET_API_KEY="your_api_key_here"
export POLYMARKET_SECRET="your_secret_here"
export POLYMARKET_PASSPHRASE="your_passphrase_here"
```

Then run:
```bash
cargo run --release -- --no-simulation
```

### Option B: Configuration File

Edit `config.json` and add your credentials:

```json
{
  "polymarket": {
    "gamma_api_url": "https://gamma-api.polymarket.com",
    "clob_api_url": "https://clob.polymarket.com",
    "ws_url": "wss://clob-ws.polymarket.com",
    "api_key": "your_api_key_here",
    "api_secret": "your_secret_here",
    "api_passphrase": "your_passphrase_here"
  },
  "trading": {
    ...
  }
}
```

⚠️ **Security Note**: Add `config.json` to `.gitignore` if it contains real credentials!

## Step 3: Verify Configuration

1. **Check your balance:**
   - Ensure you have sufficient USDC balance in your Polymarket account
   - The bot will use `max_position_size` from config as the maximum per trade

2. **Test in simulation first:**
   ```bash
   cargo run --release -- --simulation
   ```
   - Verify the bot detects opportunities correctly
   - Check logs to ensure everything works

3. **Switch to production:**
   ```bash
   cargo run --release -- --no-simulation
   ```
   - The bot will now execute real trades
   - Monitor closely for the first few trades

## Step 4: Authentication Details

### API Credentials Authentication (Required)

The bot uses **HMAC-SHA256 signature authentication** as required by Polymarket:

- **Headers sent with each authenticated request:**
  - `POLY_API_KEY`: Your API key
  - `POLY_SIGNATURE`: HMAC-SHA256 signature of the request
  - `POLY_TIMESTAMP`: Unix timestamp (requests expire after 30 seconds)
  - `POLY_PASSPHRASE`: Your passphrase

- **Signature generation:**
  - Message = `{method}{path}{body}{timestamp}`
  - Signature = HMAC-SHA256(message, secret)
  - Encoded as hexadecimal string

### Private Key (REQUIRED for Order Placement)

**CRITICAL**: According to [Polymarket's official documentation](https://docs.polymarket.com/developers/CLOB/clients/methods-l2), a **private key is REQUIRED** for placing orders:

1. **Generating API Credentials**: To create API credentials initially, you need your wallet's private key
2. **Order Signing**: **ALL orders MUST be signed with your private key** before posting (L1 authentication)
   - The signed order is then posted with API credentials (L2 authentication via HMAC-SHA256)
   - This is a two-tier authentication system

**⚠️ IMPORTANT**: The bot currently requires a private key, but the exact signing format needs to be completed. See `ORDER_SIGNING_IMPLEMENTATION.md` for details.

**To configure your private key:**

1. **Add your private key to config.json:**
   ```json
   {
     "polymarket": {
       "api_key": "...",
       "api_secret": "...",
       "api_passphrase": "...",
       "private_key": "0x..."  // Your Ethereum wallet private key (hex format)
     }
   }
   ```

2. **Or use environment variable:**
   ```bash
   export POLYMARKET_PRIVATE_KEY="0x..."
   ```

**Security Warning**: 
- ⚠️ **NEVER** commit your private key to version control
- Store it securely (environment variables, secure key management)
- The private key grants full access to your wallet funds

**Note**: The current implementation uses HMAC-SHA256 with API credentials, which should be sufficient for most operations. If you experience authentication issues, adding the private key may resolve them.

## Troubleshooting

### "API secret is required for authenticated requests"
- Make sure you've set `api_secret` in config.json or environment variables
- Verify all three credentials (key, secret, passphrase) are provided

### "Failed to decode API secret from base64"
- The secret should be base64 encoded as provided by Polymarket
- If you're using a different format, the bot will try to use it directly

### "Failed to place order (status: 401)" or "Authentication failed"
- Check that your API credentials (api_key, api_secret, api_passphrase) are correct
- Verify your account has trading permissions
- Ensure the timestamp is within 30 seconds (handled automatically)
- **If the error persists**, you may need to add your `private_key` to config.json
  - Some Polymarket operations require private key signing in addition to HMAC-SHA256
  - See "Private Key (Optional, but may be required)" section above

### "Failed to place order (status: 400)"
- Check that you have sufficient balance
- Verify the order parameters are valid
- Check Polymarket API status

## Security Best Practices

1. **Never commit credentials:**
   ```bash
   # Add to .gitignore
   echo "config.json" >> .gitignore
   ```

2. **Use environment variables in production:**
   - More secure than config files
   - Can be managed by deployment systems

3. **Rotate credentials regularly:**
   - Generate new API keys periodically
   - Revoke old keys when no longer needed

4. **Monitor your account:**
   - Check Polymarket dashboard regularly
   - Review trade history
   - Set up alerts if possible

5. **Start with small position sizes:**
   - Use `max_position_size: 5.0` or lower initially
   - Gradually increase as you gain confidence

## Additional Resources

- [Polymarket Developer Documentation](https://docs.polymarket.com/)
- [Builder Profile & Keys](https://docs.polymarket.com/developers/builders/builder-profile)
- [CLOB API Authentication](https://docs.polymarket.com/developers/CLOB/authentication)

## Support

If you encounter issues:
1. Check the logs for error messages
2. Verify your credentials are correct
3. Ensure your account has sufficient balance
4. Review Polymarket API documentation for updates

