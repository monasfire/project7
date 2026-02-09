use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(author, version, about = "Polymarket trading bot")]
pub struct Args {
    #[arg(short, long, default_value_t = true)]
    pub simulation: bool,

    #[arg(long)]
    pub production: bool,

    #[arg(short, long, default_value = "config.json")]
    pub config: PathBuf,

    #[arg(long)]
    pub redeem: bool,

    #[arg(long, requires = "redeem")]
    pub condition_id: Option<String>,
}

impl Args {
    pub fn is_simulation(&self) -> bool {
        if self.production {
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
    pub private_key: Option<String>,
    pub proxy_wallet_address: Option<String>,
    pub signature_type: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingConfig {
    pub check_interval_ms: u64,
    #[serde(default = "default_market_closure_check_interval")]
    pub market_closure_check_interval_seconds: u64,
    #[serde(default = "default_data_source")]
    pub data_source: String,
    #[serde(default = "default_markets")]
    pub markets: Vec<String>,
    #[serde(default = "default_timeframes")]
    pub timeframes: Vec<String>,
    /// Number of price datapoints required to confirm "rising" trend (e.g. 5 or 7).
    #[serde(default = "default_trend_datapoints")]
    pub trend_datapoints: usize,
    /// When market is unclear, use this many datapoints for trend (e.g. 10 or 15).
    #[serde(default = "default_trend_datapoints_extended")]
    pub trend_datapoints_extended: usize,
    /// Number of limit orders to place in one batch (e.g. 5).
    #[serde(default = "default_batch_count")]
    pub batch_count: usize,
    /// Shares per limit order in the batch (e.g. 30 → 5 orders = 150 shares total).
    #[serde(default = "default_shares_per_limit_order")]
    pub shares_per_limit_order: f64,
    /// Price offset for Down orders: place at (current_ask + this), e.g. -0.02 for 2 cents below ask.
    #[serde(default = "default_limit_order_price_offset_down")]
    pub limit_order_price_offset_down: f64,
    /// After this many seconds, cancel unmatched limit orders (production only). 0 = never cancel.
    #[serde(default = "default_cancel_unmatched_after_secs")]
    pub cancel_unmatched_after_secs: u64,
    /// Don't place orders if token ask is below this (safety).
    #[serde(default = "default_min_side_price")]
    pub min_side_price: f64,
    /// Don't place orders if token ask is above this (safety).
    #[serde(default = "default_max_side_price")]
    pub max_side_price: f64,
}

fn default_market_closure_check_interval() -> u64 {
    20
}

fn default_data_source() -> String {
    "api".to_string()
}

fn default_markets() -> Vec<String> {
    vec!["btc".to_string()]
}

fn default_timeframes() -> Vec<String> {
    vec!["15m".to_string(), "1h".to_string()]
}

fn default_trend_datapoints() -> usize {
    7
}

fn default_trend_datapoints_extended() -> usize {
    15
}

fn default_batch_count() -> usize {
    5
}

fn default_shares_per_limit_order() -> f64 {
    30.0
}

fn default_limit_order_price_offset_down() -> f64 {
    -0.02
}

fn default_cancel_unmatched_after_secs() -> u64 {
    120
}

fn default_min_side_price() -> f64 {
    0.05
}

fn default_max_side_price() -> f64 {
    0.99
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
                check_interval_ms: 1000,
                market_closure_check_interval_seconds: 20,
                data_source: "api".to_string(),
                markets: vec!["btc".to_string()],
                timeframes: default_timeframes(),
                trend_datapoints: default_trend_datapoints(),
                trend_datapoints_extended: default_trend_datapoints_extended(),
                batch_count: default_batch_count(),
                shares_per_limit_order: default_shares_per_limit_order(),
                limit_order_price_offset_down: default_limit_order_price_offset_down(),
                cancel_unmatched_after_secs: default_cancel_unmatched_after_secs(),
                min_side_price: default_min_side_price(),
                max_side_price: default_max_side_price(),
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
