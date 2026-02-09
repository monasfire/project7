mod api;
mod config;
mod models;
mod monitor;
mod trader;

use anyhow::{Context, Result};
use chrono::{Datelike, TimeZone, Timelike};
use chrono_tz::America::New_York;
use clap::Parser;
use config::{Args, Config};
use log::warn;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock, mpsc};
use std::fs::{File, OpenOptions};

use api::PolymarketApi;
use monitor::{MarketMonitor, MarketSnapshot};
use trader::Trader;
use crate::models::TokenPrice;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};

/// ANSI: move cursor to column 1. Prepended to stderr so terminal shows each line from the left.
const CURSOR_COL1: &[u8] = b"\x1b[1G";

struct DualWriter {
    stderr: io::Stderr,
    file: Mutex<File>,
}

impl Write for DualWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _guard = TERM_LOCK.lock().unwrap();
        let mut stderr_buf = Vec::with_capacity(buf.len() + 64);
        if !buf.is_empty() && buf[0] != b'\r' {
            stderr_buf.push(b'\r');
        }
        for (i, &b) in buf.iter().enumerate() {
            if b == b'\n' && (i == 0 || buf[i - 1] != b'\r') {
                stderr_buf.push(b'\r');
            }
            stderr_buf.push(b);
        }
        let _ = self.stderr.write_all(&stderr_buf);
        let _ = self.stderr.flush();
        let mut file = self.file.lock().unwrap();
        file.write_all(buf)?;
        file.flush()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _guard = TERM_LOCK.lock().unwrap();
        self.stderr.flush()?;
        let mut file = self.file.lock().unwrap();
        file.flush()?;
        Ok(())
    }
}

unsafe impl Send for DualWriter {}
unsafe impl Sync for DualWriter {}

static HISTORY_FILE: OnceLock<Mutex<File>> = OnceLock::new();
/// Serializes all terminal output so price feed and log lines don't interleave.
static TERM_LOCK: Mutex<()> = Mutex::new(());

fn init_history_file(file: File) {
    HISTORY_FILE.set(Mutex::new(file)).expect("History file already initialized");
}

pub fn log_to_history(message: &str) {
    let _guard = TERM_LOCK.lock().unwrap();
    let mut term_msg = String::with_capacity(message.len() + 64);
    term_msg.push_str(std::str::from_utf8(CURSOR_COL1).unwrap_or(""));
    for (i, c) in message.chars().enumerate() {
        term_msg.push(c);
        if c == '\n' && i + 1 < message.len() {
            term_msg.push_str(std::str::from_utf8(CURSOR_COL1).unwrap_or(""));
        }
    }
    eprint!("{}", term_msg);
    let _ = io::stderr().flush();
    if let Some(file_mutex) = HISTORY_FILE.get() {
        if let Ok(mut file) = file_mutex.lock() {
            let _ = write!(file, "{}", message);
            let _ = file.flush();
        }
    }
}

fn format_token_price(p: &TokenPrice) -> String {
    let bid = p.bid.as_ref().map(|d| d.to_string().parse::<f64>().unwrap_or(0.0)).unwrap_or(0.0);
    let ask = p.ask.as_ref().map(|d| d.to_string().parse::<f64>().unwrap_or(0.0)).unwrap_or(0.0);
    format!("BID:${:.2} ASK:${:.2}", bid, ask)
}

fn format_remaining_time(secs: u64) -> String {
    if secs >= 3600 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        format!("{}h {}m {}s", h, m, s)
    } else if secs >= 60 {
        let m = secs / 60;
        let s = secs % 60;
        format!("{}m {}s", m, s)
    } else {
        format!("{}s", secs)
    }
}

async fn handle_key(
    c: char,
    trader: Arc<Trader>,
    last_snapshot: Arc<tokio::sync::Mutex<Option<MarketSnapshot>>>,
) {
    let guard = last_snapshot.lock().await;
    let snapshot = match guard.as_ref() {
        Some(s) => s.clone(),
        None => {
            crate::log_println!("No snapshot yet; wait for price feed.");
            return;
        }
    };
    drop(guard);
    match c {
        '+' => {
            if let Err(e) = trader.place_up_batch(&snapshot).await {
                warn!("Place Up batch failed: {}", e);
            }
        }
        '-' => {
            if let Err(e) = trader.place_down_batch(&snapshot).await {
                warn!("Place Down batch failed: {}", e);
            }
        }
        '*' => {
            if let Err(e) = trader.cancel_up_orders(&snapshot).await {
                warn!("Cancel Up orders failed: {}", e);
            }
        }
        '/' => {
            if let Err(e) = trader.cancel_down_orders(&snapshot).await {
                warn!("Cancel Down orders failed: {}", e);
            }
        }
        _ => {}
    }
}

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
    let history_path = "history.toml";
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(history_path)
        .context("Failed to open history.toml for logging")?;

    let log_file_for_writer = OpenOptions::new()
        .create(true)
        .append(true)
        .open(history_path)
        .context("Failed to open history.toml for writer")?;

    init_history_file(log_file);

    let dual_writer = DualWriter {
        stderr: io::stderr(),
        file: Mutex::new(log_file_for_writer),
    };

    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .format(|buf, record| {
            use std::io::Write;
            write!(buf, "\r[{}] {}\r\n", record.level(), record.args())
        })
        .target(env_logger::Target::Pipe(Box::new(dual_writer)))
        .init();

    let args = Args::parse();
    let config = Config::load(&args.config)?;

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

    if args.redeem {
        run_redeem_only(api.as_ref(), &config, args.condition_id.as_deref()).await?;
        return Ok(());
    }

    let is_simulation = args.is_simulation();
    eprintln!("Starting Polymarket Trading Bot");
    eprintln!("Mode: {}", if is_simulation { "SIMULATION" } else { "PRODUCTION" });
    if is_simulation {
        eprintln!("PnL will be calculated after each market closes and winner is known.");
    }

    if !is_simulation {
        match api.authenticate().await {
            Ok(_) => crate::log_println!("Authentication successful"),
            Err(e) => {
                warn!("Failed to authenticate: {}", e);
                warn!("Order placement may fail. Verify credentials in config.json");
            }
        }
    }

    let markets = &config.trading.markets;
    if markets.is_empty() {
        anyhow::bail!("No markets configured. Add markets to config (e.g. [\"btc\", \"eth\"])");
    }

    let batch_count = config.trading.batch_count;
    let shares_per_limit_order = config.trading.shares_per_limit_order;
    let min_side_price = config.trading.min_side_price;
    let max_side_price = config.trading.max_side_price;
    let data_source = config.trading.data_source.clone();

    let timeframes = &config.trading.timeframes;
    let timeframes_str: Vec<&str> = timeframes.iter().map(|s| s.as_str()).collect();
    crate::log_println!("Mode: Human-interactive (monitor price feed; single keypress = action)");
    crate::log_println!("   Markets: {}", markets.join(", ").to_uppercase());
    crate::log_println!("   Timeframes: {}", timeframes_str.join(", "));
    crate::log_println!("   Batch: {} x {} shares per limit order (limit = ask+0.01)", batch_count, shares_per_limit_order);
    crate::log_println!("   Price bounds: ${:.2}–${:.2}", min_side_price, max_side_price);
    crate::log_println!("   Data source: {}", data_source.to_uppercase());
    crate::log_println!("   Keys: + = place Up batch  |  - = place Down batch  |  * = cancel Up orders  |  / = cancel Down orders");
    crate::log_println!("");

    let trader = Arc::new(Trader::new(
        api.clone(),
        is_simulation,
        batch_count,
        shares_per_limit_order,
        min_side_price,
        max_side_price,
    ));
    let trader_closure = trader.clone();
    let market_closure_interval = config.trading.market_closure_check_interval_seconds;

    let last_snapshot: Arc<tokio::sync::Mutex<Option<MarketSnapshot>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    let trader_kb = trader.clone();
    let last_snapshot_kb = last_snapshot.clone();
    let (tx, rx) = mpsc::sync_channel::<char>(8);
    std::thread::spawn(move || {
        use std::time::Duration;
        loop {
            if crossterm::terminal::enable_raw_mode().is_err() {
                break;
            }
            let has_event = event::poll(Duration::from_millis(80)).unwrap_or(false);
            if has_event {
                match event::read() {
                    Ok(Event::Key(ke)) if ke.kind == KeyEventKind::Press => {
                        if ke.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) && ke.code == KeyCode::Char('c') {
                            let _ = crossterm::terminal::disable_raw_mode();
                            std::process::exit(0);
                        }
                        let c = match ke.code {
                            KeyCode::Char('+') => '+',
                            KeyCode::Char('-') => '-',
                            KeyCode::Char('*') => '*',
                            KeyCode::Char('/') => '/',
                            _ => continue,
                        };
                        if tx.send(c).is_err() {
                            let _ = crossterm::terminal::disable_raw_mode();
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => {}
                }
            }
            let _ = crossterm::terminal::disable_raw_mode();
            std::thread::sleep(Duration::from_millis(85));
        }
    });
    let handle = tokio::runtime::Handle::current();
    tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || {
            while let Ok(c) = rx.recv() {
                handle.block_on(handle_key(c, trader_kb.clone(), last_snapshot_kb.clone()));
            }
        }).await;
    });

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(market_closure_interval));
        loop {
            interval.tick().await;
            if let Err(e) = trader_closure.check_market_closure().await {
                warn!("Error checking market closure: {}", e);
            }
            let total_profit = trader_closure.get_total_profit().await;
            let period_profit = trader_closure.get_period_profit().await;
            if total_profit != 0.0 || period_profit != 0.0 {
                crate::log_println!("Current Profit - Period: ${:.2} | Total: ${:.2}", period_profit, total_profit);
            }
        }
    });

    let mut handles = Vec::new();
    for asset in markets {
        for timeframe in timeframes {
            let asset_upper = asset.to_uppercase();
            let tf = timeframe.trim().to_lowercase();
            let market_name = format!("{} {}", asset_upper, timeframe);
            let duration_minutes = if tf == "1h" { 60 } else { 15 };
            let period_secs: u64 = if tf == "1h" { 3600 } else { 900 };

            crate::log_println!("Discovering {} market...", market_name);
            let market = match discover_market_for_asset_timeframe(&api, asset, duration_minutes).await {
                Ok(m) => m,
                Err(e) => {
                    warn!("Failed to discover {} market: {}. Skipping...", market_name, e);
                    continue;
                }
            };

            let monitor = MarketMonitor::new(
                api.clone(),
                market_name.clone(),
                market,
                config.trading.check_interval_ms,
                data_source.clone(),
                config.polymarket.clob_api_url.clone(),
            );
            let monitor_arc = Arc::new(monitor);

            let monitor_for_period_check = monitor_arc.clone();
            let api_for_period_check = api.clone();
            let trader_for_period_reset = trader.clone();
            let asset_owned = asset.to_string();
            let market_name_owned = market_name.clone();
            let timeframe_owned = timeframe.clone();

            let handle = tokio::spawn(async move {
                let mut last_processed_period: Option<u64> = None;
                loop {
                    let current_market_timestamp = monitor_for_period_check.get_current_market_timestamp().await;
                    let next_period_timestamp = current_market_timestamp + period_secs;
                    let current_time = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    let sleep_duration = if next_period_timestamp > current_time {
                        next_period_timestamp - current_time
                    } else {
                        0
                    };
                    tokio::time::sleep(tokio::time::Duration::from_secs(sleep_duration)).await;
                    let current_time = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    let current_period = (current_time / period_secs) * period_secs;
                    if let Some(last_period) = last_processed_period {
                        if current_period == last_period {
                            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                            continue;
                        }
                    }
                    crate::log_println!("New period detected for {}! (Period: {}) Discovering new market...", market_name_owned, current_period);
                    last_processed_period = Some(current_period);
                    let duration_min = if timeframe_owned.trim().eq_ignore_ascii_case("1h") { 60 } else { 15 };
                    match discover_market_for_asset_timeframe(&api_for_period_check, &asset_owned, duration_min).await {
                        Ok(new_market) => {
                            if let Err(e) = monitor_for_period_check.update_market(new_market).await {
                                warn!("Failed to update {} market: {}", market_name_owned, e);
                            } else {
                                trader_for_period_reset.reset_period().await;
                            }
                        }
                        Err(e) => {
                            warn!("Failed to discover new {} market: {}", market_name_owned, e);
                            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                        }
                    }
                }
            });
            handles.push(handle);

            let monitor_start = monitor_arc.clone();
            let trader_start = trader.clone();
            let last_snapshot_cb = last_snapshot.clone();
            tokio::spawn(async move {
                monitor_start
                    .start_monitoring(move |snapshot| {
                        let trader = trader_start.clone();
                        let last_snapshot = last_snapshot_cb.clone();
                        async move {
                            last_snapshot.lock().await.replace(snapshot.clone());
                            if let Err(e) = trader.process_snapshot(&snapshot).await {
                                warn!("Error processing snapshot: {}", e);
                            }
                            let market_key = format!("{}:{}", snapshot.btc_market_15m.condition_id, snapshot.btc_15m_period_timestamp);
                            let (up_bought, down_bought) = trader.get_matched_bought_prices(&market_key).await;
                            let up_str = snapshot.btc_market_15m.up_token.as_ref().map(format_token_price).unwrap_or_else(|| "N/A".to_string());
                            let down_str = snapshot.btc_market_15m.down_token.as_ref().map(format_token_price).unwrap_or_else(|| "N/A".to_string());
                            let remaining = format_remaining_time(snapshot.btc_15m_time_remaining);
                            let up_bought_str: String = if up_bought.is_empty() { "—".to_string() } else { up_bought.iter().map(|p| format!("${:.2}", p)).collect::<Vec<_>>().join(" ") };
                            let down_bought_str: String = if down_bought.is_empty() { "—".to_string() } else { down_bought.iter().map(|p| format!("${:.2}", p)).collect::<Vec<_>>().join(" ") };
                            let message = format!(
                                "{} Up {} Down {} ⏳ {} | Up bought: {} | Down bought: {}\n",
                                snapshot.market_name, up_str, down_str, remaining, up_bought_str, down_bought_str
                            );
                            crate::log_to_history(&message);
                        }
                    })
                    .await;
            });
        }
    }

    if handles.is_empty() {
        anyhow::bail!("No valid markets found. Check your market configuration.");
    }

    crate::log_println!("Started monitoring {} market(s)", handles.len());
    futures::future::join_all(handles).await;
    Ok(())
}

async fn run_redeem_only(
    api: &PolymarketApi,
    config: &Config,
    condition_id: Option<&str>,
) -> Result<()> {
    let proxy = config
        .polymarket
        .proxy_wallet_address
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--redeem requires proxy_wallet_address in config.json"))?;

    eprintln!("Redeem-only mode (proxy: {})", proxy);
    let cids: Vec<String> = if let Some(cid) = condition_id {
        let cid = if cid.starts_with("0x") { cid.to_string() } else { format!("0x{}", cid) };
        eprintln!("Redeeming condition: {}", cid);
        vec![cid]
    } else {
        eprintln!("Fetching redeemable positions...");
        let list = api.get_redeemable_positions(proxy).await?;
        if list.is_empty() {
            eprintln!("No redeemable positions found.");
            return Ok(());
        }
        eprintln!("Found {} condition(s) to redeem.", list.len());
        list
    };

    let mut ok_count = 0u32;
    let mut fail_count = 0u32;
    for cid in &cids {
        eprintln!("\n--- Redeeming condition {} ---", &cid[..cid.len().min(18)]);
        match api.redeem_tokens(cid, "", "Up").await {
            Ok(_) => {
                eprintln!("Success: {}", cid);
                ok_count += 1;
            }
            Err(e) => {
                eprintln!("Failed to redeem {}: {} (skipping)", cid, e);
                fail_count += 1;
            }
        }
    }
    eprintln!("\nRedeem complete. Succeeded: {}, Failed: {}", ok_count, fail_count);
    Ok(())
}

async fn discover_market_for_asset_timeframe(
    api: &PolymarketApi,
    asset: &str,
    market_duration_minutes: u64,
) -> Result<crate::models::Market> {
    let current_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut seen_ids = std::collections::HashSet::new();
    let asset_lower = asset.to_lowercase();
    let slug_prefix = match asset_lower.as_str() {
        "btc" => "btc",
        "eth" => "eth",
        "sol" => "sol",
        "xrp" => "xrp",
        _ => anyhow::bail!("Unsupported asset: {}. Supported: BTC, ETH, SOL, XRP", asset),
    };
    let timeframe_str = if market_duration_minutes == 60 { "1h" } else { "15m" };
    discover_market(api, asset, slug_prefix, market_duration_minutes, current_time, &mut seen_ids).await
        .context(format!("Failed to discover {} {} market", asset, timeframe_str))
}

/// Build 1h market slug in Polymarket format: bitcoin-up-or-down-february-2-11pm-et
fn slug_1h_human_readable(period_start_unix: u64, slug_prefix: &str) -> String {
    let dt_utc = chrono::Utc.timestamp_opt(period_start_unix as i64, 0).single().unwrap();
    let dt_et = dt_utc.with_timezone(&New_York);
    let month = match dt_et.month() {
        1 => "january",
        2 => "february",
        3 => "march",
        4 => "april",
        5 => "may",
        6 => "june",
        7 => "july",
        8 => "august",
        9 => "september",
        10 => "october",
        11 => "november",
        12 => "december",
        _ => "january",
    };
    let day = dt_et.day();
    let hour_24 = dt_et.hour();
    let (hour_12, am_pm) = if hour_24 == 0 {
        (12, "am")
    } else if hour_24 < 12 {
        (hour_24, "am")
    } else if hour_24 == 12 {
        (12, "pm")
    } else {
        (hour_24 - 12, "pm")
    };
    let asset_name = match slug_prefix {
        "btc" => "bitcoin",
        "eth" => "ethereum",
        _ => slug_prefix,
    };
    format!(
        "{}-up-or-down-{}-{}-{}{}-et",
        asset_name, month, day, hour_12, am_pm
    )
}

async fn discover_market(
    api: &PolymarketApi,
    market_name: &str,
    slug_prefix: &str,
    market_duration_minutes: u64,
    current_time: u64,
    seen_ids: &mut std::collections::HashSet<String>,
) -> Result<crate::models::Market> {
    let (period_duration_secs, timeframe_str) = if market_duration_minutes == 60 {
        (3600u64, "1h")
    } else if market_duration_minutes == 15 {
        (900u64, "15m")
    } else {
        anyhow::bail!("Only 15m and 1h markets are supported, got {}m", market_duration_minutes);
    };
    let rounded_time = (current_time / period_duration_secs) * period_duration_secs;

    let slug = if market_duration_minutes == 60 {
        slug_1h_human_readable(rounded_time, slug_prefix)
    } else {
        format!("{}-updown-15m-{}", slug_prefix, rounded_time)
    };

    if let Ok(market) = api.get_market_by_slug(&slug).await {
        if !seen_ids.contains(&market.condition_id) && market.active && !market.closed {
            crate::log_println!("Found {} {} market by slug: {} | Condition ID: {}",
                market_name, timeframe_str, market.slug, market.condition_id
            );
            return Ok(market);
        }
    }

    for offset in 1..=3 {
        let try_time = rounded_time - (offset * period_duration_secs);
        let try_slug = if market_duration_minutes == 60 {
            slug_1h_human_readable(try_time, slug_prefix)
        } else {
            format!("{}-updown-15m-{}", slug_prefix, try_time)
        };
        eprintln!("Trying previous {} {} market by slug: {}", market_name, timeframe_str, try_slug);
        if let Ok(market) = api.get_market_by_slug(&try_slug).await {
            if !seen_ids.contains(&market.condition_id) && market.active && !market.closed {
                eprintln!(
                    "Found {} {} market by slug: {} | Condition ID: {}",
                    market_name, timeframe_str, market.slug, market.condition_id
                );
                return Ok(market);
            }
        }
    }

    anyhow::bail!(
        "Could not find active {} {} up/down market. Set condition_id in config if needed.",
        market_name,
        timeframe_str
    )
}
