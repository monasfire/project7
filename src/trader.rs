//! Human-interactive trading:
//! - Price feed is shown in terminal; human decides when to act.
//! - + = place Up batch (limit buy at ask+0.01), - = place Down batch, * = cancel Up orders, / = cancel Down orders.
//! - Matched: in sim when market price passes limit; in prod verified via fills API. Display positions and PnL.

use crate::api::PolymarketApi;
use crate::monitor::MarketSnapshot;
use anyhow::Result;
use log::warn;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

const MATCH_EPSILON: f64 = 0.0001;

#[derive(Debug, Clone)]
struct PendingOrder {
    side: String,
    price: f64,
    size: f64,
    order_id: Option<String>,
    matched: bool,
}

#[derive(Debug, Clone)]
struct PendingBatch {
    placed_at: u64,
    orders: Vec<PendingOrder>,
}

pub struct Trader {
    api: Arc<PolymarketApi>,
    simulation_mode: bool,
    batch_count: usize,
    shares_per_limit_order: f64,
    min_side_price: f64,
    max_side_price: f64,
    /// market_key -> batches of placed orders (Up and/or Down)
    pending_batches: Arc<Mutex<HashMap<String, std::collections::VecDeque<PendingBatch>>>>,
    last_pnl_log: Arc<Mutex<HashMap<String, (f64, f64, usize, f64)>>>,
    last_fill_sync: Arc<Mutex<HashMap<String, u64>>>,
    trades: Arc<Mutex<HashMap<String, CycleTrade>>>,
    total_profit: Arc<Mutex<f64>>,
    period_profit: Arc<Mutex<f64>>,
    closure_checked: Arc<Mutex<HashMap<String, bool>>>,
}

#[derive(Debug, Clone)]
struct CycleTrade {
    condition_id: String,
    period_timestamp: u64,
    market_duration_secs: u64,
    up_token_id: Option<String>,
    down_token_id: Option<String>,
    up_shares: f64,
    down_shares: f64,
    up_avg_price: f64,
    down_avg_price: f64,
}

impl Trader {
    pub fn new(
        api: Arc<PolymarketApi>,
        simulation_mode: bool,
        batch_count: usize,
        shares_per_limit_order: f64,
        min_side_price: f64,
        max_side_price: f64,
    ) -> Self {
        Self {
            api,
            simulation_mode,
            batch_count,
            shares_per_limit_order,
            min_side_price,
            max_side_price,
            pending_batches: Arc::new(Mutex::new(HashMap::new())),
            last_pnl_log: Arc::new(Mutex::new(HashMap::new())),
            last_fill_sync: Arc::new(Mutex::new(HashMap::new())),
            trades: Arc::new(Mutex::new(HashMap::new())),
            total_profit: Arc::new(Mutex::new(0.0)),
            period_profit: Arc::new(Mutex::new(0.0)),
            closure_checked: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Process each snapshot: update matched from price (sim) or rely on fill sync (prod), then log position/PnL when changed.
    pub async fn process_snapshot(&self, snapshot: &MarketSnapshot) -> Result<()> {
        let market_name = &snapshot.market_name;
        let market_data = &snapshot.btc_market_15m;
        let period_timestamp = snapshot.btc_15m_period_timestamp;
        let condition_id = &market_data.condition_id;
        let time_remaining = snapshot.btc_15m_time_remaining;

        if time_remaining == 0 {
            return Ok(());
        }

        let up_ask = market_data
            .up_token
            .as_ref()
            .and_then(|t| t.ask_price().to_string().parse::<f64>().ok())
            .unwrap_or(0.0);
        let down_ask = market_data
            .down_token
            .as_ref()
            .and_then(|t| t.ask_price().to_string().parse::<f64>().ok())
            .unwrap_or(0.0);

        if up_ask <= 0.0 || down_ask <= 0.0 {
            return Ok(());
        }

        let market_key = format!("{}:{}", condition_id, period_timestamp);
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Update matched: current ask <= order price => filled (sim and prod use this for display; prod also syncs from API)
        {
            let mut batches = self.pending_batches.lock().await;
            if let Some(batch_list) = batches.get_mut(&market_key) {
                for batch in batch_list.iter_mut() {
                    for o in batch.orders.iter_mut() {
                        if o.matched {
                            continue;
                        }
                        let filled = if o.side.eq_ignore_ascii_case("Up") {
                            up_ask <= o.price + MATCH_EPSILON
                        } else {
                            down_ask <= o.price + MATCH_EPSILON
                        };
                        if filled {
                            o.matched = true;
                        }
                    }
                }
            }
        }

        self.log_matched_and_pnl(market_name, &market_key, up_ask, down_ask).await;

        // Production: periodically sync position from fills API to confirm matched
        const FILL_SYNC_INTERVAL_SECS: u64 = 20;
        if !self.simulation_mode {
            let do_sync = {
                let mut sync_key = self.last_fill_sync.lock().await;
                let do_sync = sync_key
                    .get(&market_key)
                    .map(|&t| current_time >= t.saturating_add(FILL_SYNC_INTERVAL_SECS))
                    .unwrap_or(true);
                if do_sync {
                    sync_key.insert(market_key.clone(), current_time);
                }
                do_sync
            };
            if do_sync {
                self.sync_position_and_log_pnl(
                    market_name,
                    condition_id,
                    &market_key,
                    period_timestamp,
                    snapshot.market_duration_secs,
                    market_data,
                )
                .await;
            }
        }

        Ok(())
    }

    pub async fn place_up_batch(&self, snapshot: &MarketSnapshot) -> Result<()> {
        self.place_side_batch(snapshot, "Up").await
    }

    pub async fn place_down_batch(&self, snapshot: &MarketSnapshot) -> Result<()> {
        self.place_side_batch(snapshot, "Down").await
    }

    async fn place_side_batch(&self, snapshot: &MarketSnapshot, side: &str) -> Result<()> {
        let market_name = &snapshot.market_name;
        let market_data = &snapshot.btc_market_15m;
        let period_timestamp = snapshot.btc_15m_period_timestamp;
        let condition_id = &market_data.condition_id;
        let market_key = format!("{}:{}", condition_id, period_timestamp);
        let duration_secs = snapshot.market_duration_secs;

        let (token_id, ask) = if side.eq_ignore_ascii_case("Up") {
            let tid = market_data.up_token.as_ref().map(|t| t.token_id.clone());
            let a = market_data
                .up_token
                .as_ref()
                .and_then(|t| t.ask_price().to_string().parse::<f64>().ok())
                .unwrap_or(0.0);
            (tid, a)
        } else {
            let tid = market_data.down_token.as_ref().map(|t| t.token_id.clone());
            let a = market_data
                .down_token
                .as_ref()
                .and_then(|t| t.ask_price().to_string().parse::<f64>().ok())
                .unwrap_or(0.0);
            (tid, a)
        };

        let token_id = match token_id {
            Some(id) => id,
            None => return Ok(()),
        };
        if ask <= 0.0 {
            return Ok(());
        }

        let price = (ask + 0.01).min(self.max_side_price).max(self.min_side_price);
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        crate::log_println!(
            "📊 {}: placing {} limit buy {} @ ${:.4} x {} shares each (ask+0.01)",
            market_name, self.batch_count, side, price, self.shares_per_limit_order
        );
        let ids = self
            .place_limit_order_batch(market_name, side, &token_id, price, duration_secs)
            .await?;

        let size = self.shares_per_limit_order;
        let mut orders = Vec::with_capacity(self.batch_count);
        for i in 0..self.batch_count {
            orders.push(PendingOrder {
                side: side.to_string(),
                price,
                size,
                order_id: ids.get(i).cloned(),
                matched: false,
            });
        }
        let batch = PendingBatch {
            placed_at: current_time,
            orders,
        };
        self.pending_batches
            .lock()
            .await
            .entry(market_key)
            .or_default()
            .push_back(batch);

        Ok(())
    }

    /// Cancel all open Up limit orders for the market in the snapshot (production: API cancel; sim: remove from tracking).
    pub async fn cancel_up_orders(&self, snapshot: &MarketSnapshot) -> Result<()> {
        self.cancel_side_orders(snapshot, "Up").await
    }

    /// Cancel all open Down limit orders for the market in the snapshot.
    pub async fn cancel_down_orders(&self, snapshot: &MarketSnapshot) -> Result<()> {
        self.cancel_side_orders(snapshot, "Down").await
    }

    async fn cancel_side_orders(&self, snapshot: &MarketSnapshot, side: &str) -> Result<()> {
        let market_name = &snapshot.market_name;
        let market_data = &snapshot.btc_market_15m;
        let period_timestamp = snapshot.btc_15m_period_timestamp;
        let condition_id = &market_data.condition_id;
        let market_key = format!("{}:{}", condition_id, period_timestamp);

        let to_cancel: Vec<String> = {
            let mut batches = self.pending_batches.lock().await;
            let list = match batches.get_mut(&market_key) {
                Some(l) => l,
                None => {
                    crate::log_println!("  {} | No pending orders to cancel for {}", market_name, side);
                    return Ok(());
                }
            };
            let mut ids = Vec::new();
            for batch in list.iter() {
                for o in &batch.orders {
                    if o.side.eq_ignore_ascii_case(side) && !o.matched {
                        if let Some(ref id) = o.order_id {
                            ids.push(id.clone());
                        }
                    }
                }
            }
            ids
        };

        if to_cancel.is_empty() {
            crate::log_println!("  {} | No open {} orders to cancel", market_name, side);
            return Ok(());
        }

        if !self.simulation_mode {
            if let Err(e) = self.api.cancel_orders(&to_cancel).await {
                warn!("Failed to cancel {} orders: {}", side, e);
                return Err(e.into());
            }
        }
        crate::log_println!("  {} | Cancelled {} {} order(s)", market_name, to_cancel.len(), side);

        // Remove those orders from tracking (drop unmatched orders of this side)
        let mut batches = self.pending_batches.lock().await;
        if let Some(list) = batches.get_mut(&market_key) {
            for batch in list.iter_mut() {
                batch.orders.retain(|o| !(o.side.eq_ignore_ascii_case(side) && !o.matched));
            }
            list.retain(|b| !b.orders.is_empty());
        }
        Ok(())
    }

    async fn log_matched_and_pnl(&self, market_name: &str, market_key: &str, _up_ask: f64, _down_ask: f64) {
        let state = {
            let batches = self.pending_batches.lock().await;
            let (mut up_shares, mut down_shares, mut total_deposit, mut unmatched) =
                (0.0_f64, 0.0_f64, 0.0_f64, 0usize);
            if let Some(list) = batches.get(market_key) {
                for batch in list.iter() {
                    for o in &batch.orders {
                        if o.matched {
                            total_deposit += o.price * o.size;
                            if o.side.eq_ignore_ascii_case("Up") {
                                up_shares += o.size;
                            } else {
                                down_shares += o.size;
                            }
                        } else {
                            unmatched += 1;
                        }
                    }
                }
            }
            (up_shares, down_shares, unmatched, total_deposit)
        };
        let total_matched = state.0 + state.1;
        if total_matched <= 0.0 && state.2 == 0 {
            return;
        }
        let last = self.last_pnl_log.lock().await;
        let same = last
            .get(market_key)
            .map(|&(a, b, c, d)| {
                (state.0 - a).abs() < 1e-6
                    && (state.1 - b).abs() < 1e-6
                    && state.2 == c
                    && (state.3 - d).abs() < 1e-6
            })
            .unwrap_or(false);
        if same {
            return;
        }
        drop(last);
        {
            let mut last = self.last_pnl_log.lock().await;
            last.insert(market_key.to_string(), (state.0, state.1, state.2, state.3));
        }
        let pnl_up = state.0 - state.3;
        let pnl_down = state.1 - state.3;
        crate::log_println!(
            "  {} | Holding: {:.0} Up, {:.0} Down | Unmatched: {} | Total deposit: ${:.2} | PnL if Up wins: ${:.2} | if Down wins: ${:.2}",
            market_name, state.0, state.1, state.2, state.3, pnl_up, pnl_down
        );
    }

    async fn sync_position_and_log_pnl(
        &self,
        market_name: &str,
        condition_id: &str,
        market_key: &str,
        period_timestamp: u64,
        market_duration_secs: u64,
        market_data: &crate::models::MarketData,
    ) {
        let up_token_id = market_data.up_token.as_ref().map(|t| t.token_id.as_str());
        let down_token_id = market_data.down_token.as_ref().map(|t| t.token_id.as_str());
        let up_token_id_owned = up_token_id.map(|s| s.to_string());
        let down_token_id_owned = down_token_id.map(|s| s.to_string());

        let user = match self.api.get_trading_address() {
            Ok(u) => u,
            Err(_) => return,
        };

        let fills = match self
            .api
            .get_user_fills_for_market(&user, condition_id, Some(200))
            .await
        {
            Ok(f) => f,
            Err(e) => {
                log::debug!("{} fill sync failed: {}", market_name, e);
                return;
            }
        };

        let mut up_shares = 0.0_f64;
        let mut down_shares = 0.0_f64;
        let mut up_cost = 0.0_f64;
        let mut down_cost = 0.0_f64;

        for fill in &fills {
            if fill.side.to_uppercase() != "BUY" {
                continue;
            }
            let size = fill.size;
            let cost = size * fill.price;
            let is_up = fill
                .outcome
                .as_deref()
                .map(|o| o.eq_ignore_ascii_case("up"))
                .unwrap_or_else(|| {
                    fill
                        .get_token_id()
                        .map(|tid| up_token_id == Some(tid.as_str()))
                        .unwrap_or(false)
                });
            let is_down = fill
                .outcome
                .as_deref()
                .map(|o| o.eq_ignore_ascii_case("down"))
                .unwrap_or_else(|| {
                    fill
                        .get_token_id()
                        .map(|tid| down_token_id == Some(tid.as_str()))
                        .unwrap_or(false)
                });
            if is_up {
                up_shares += size;
                up_cost += cost;
            } else if is_down {
                down_shares += size;
                down_cost += cost;
            }
        }

        let total_cost = up_cost + down_cost;
        let pnl_if_up_wins = up_shares - total_cost;
        let pnl_if_down_wins = down_shares - total_cost;

        if up_shares > 0.0 || down_shares > 0.0 {
            crate::log_println!(
                "  {} | Position (from API): Up {:.2} @ ${:.2} cost | Down {:.2} @ ${:.2} cost | Total ${:.2} | PnL if Up wins: ${:.2} | if Down wins: ${:.2}",
                market_name,
                up_shares,
                up_cost,
                down_shares,
                down_cost,
                total_cost,
                pnl_if_up_wins,
                pnl_if_down_wins
            );
        }

        if up_shares > 0.0 || down_shares > 0.0 {
            let up_avg = if up_shares > 0.0 {
                up_cost / up_shares
            } else {
                0.0
            };
            let down_avg = if down_shares > 0.0 {
                down_cost / down_shares
            } else {
                0.0
            };
            let mut trades = self.trades.lock().await;
            trades.insert(
                market_key.to_string(),
                CycleTrade {
                    condition_id: condition_id.to_string(),
                    period_timestamp,
                    market_duration_secs,
                    up_token_id: up_token_id_owned,
                    down_token_id: down_token_id_owned,
                    up_shares,
                    down_shares,
                    up_avg_price: up_avg,
                    down_avg_price: down_avg,
                },
            );
        }
    }

    async fn place_limit_order_batch(
        &self,
        market_name: &str,
        side: &str,
        token_id: &str,
        price: f64,
        _duration_secs: u64,
    ) -> Result<Vec<String>> {
        let mut order_ids = Vec::with_capacity(self.batch_count);
        let size = self.shares_per_limit_order;
        let price_str = format!("{:.4}", price);

        for i in 0..self.batch_count {
            if self.simulation_mode {
                crate::log_println!(
                    "  [SIM] Limit BUY {} #{}/{} @ ${} x {}",
                    side,
                    i + 1,
                    self.batch_count,
                    price_str,
                    size
                );
                continue;
            }
            let order = crate::models::OrderRequest {
                token_id: token_id.to_string(),
                side: "BUY".to_string(),
                size: format!("{:.4}", size),
                price: price_str.clone(),
                order_type: "LIMIT".to_string(),
            };
            match self.api.place_order(&order).await {
                Ok(resp) => {
                    if let Some(id) = resp.order_id {
                        crate::log_println!("  {} limit order #{} placed: {}", market_name, i + 1, id);
                        order_ids.push(id);
                    }
                }
                Err(e) => {
                    warn!("Failed to place limit order #{}: {}", i + 1, e);
                }
            }
        }
        Ok(order_ids)
    }

    pub async fn check_market_closure(&self) -> Result<()> {
        let trades: Vec<(String, CycleTrade)> = {
            let t = self.trades.lock().await;
            t.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        if trades.is_empty() {
            return Ok(());
        }
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        for (market_key, trade) in trades {
            let market_end = trade.period_timestamp + trade.market_duration_secs;
            if current_time < market_end {
                continue;
            }

            let checked = self.closure_checked.lock().await;
            if checked.get(&trade.condition_id).copied().unwrap_or(false) {
                drop(checked);
                continue;
            }
            drop(checked);

            let market = match self.api.get_market(&trade.condition_id).await {
                Ok(m) => m,
                Err(e) => {
                    warn!("Failed to fetch market {}: {}", &trade.condition_id[..16], e);
                    continue;
                }
            };
            if !market.closed {
                continue;
            }

            let up_wins = trade
                .up_token_id
                .as_ref()
                .map(|id| market.tokens.iter().any(|t| t.token_id == *id && t.winner))
                .unwrap_or(false);
            let down_wins = trade
                .down_token_id
                .as_ref()
                .map(|id| market.tokens.iter().any(|t| t.token_id == *id && t.winner))
                .unwrap_or(false);

            let total_cost =
                (trade.up_shares * trade.up_avg_price) + (trade.down_shares * trade.down_avg_price);
            let payout = if up_wins {
                trade.up_shares * 1.0
            } else if down_wins {
                trade.down_shares * 1.0
            } else {
                0.0
            };
            let pnl = payout - total_cost;

            let winner = if up_wins {
                "Up"
            } else if down_wins {
                "Down"
            } else {
                "Unknown"
            };
            crate::log_println!("=== Market resolved ===");
            crate::log_println!(
                "Market closed | condition {} | Winner: {} | Up {:.2} @ {:.4} | Down {:.2} @ {:.4} | Cost ${:.2} | Payout ${:.2} | Actual PnL ${:.2}",
                &trade.condition_id[..16],
                winner,
                trade.up_shares,
                trade.up_avg_price,
                trade.down_shares,
                trade.down_avg_price,
                total_cost,
                payout,
                pnl
            );

            if !self.simulation_mode && (up_wins || down_wins) {
                let (token_id, outcome) = if up_wins && trade.up_shares > 0.001 {
                    (trade.up_token_id.as_deref().unwrap_or(""), "Up")
                } else {
                    (trade.down_token_id.as_deref().unwrap_or(""), "Down")
                };
                if let Err(e) = self
                    .api
                    .redeem_tokens(&trade.condition_id, token_id, outcome)
                    .await
                {
                    warn!("Redeem failed: {}", e);
                }
            }

            {
                let mut total = self.total_profit.lock().await;
                *total += pnl;
            }
            {
                let mut period = self.period_profit.lock().await;
                *period += pnl;
            }
            let total_actual_pnl = *self.total_profit.lock().await;
            crate::log_println!(
                "  -> Actual PnL this market: ${:.2} | Total actual PnL (all time): ${:.2}",
                pnl,
                total_actual_pnl
            );
            {
                let mut c = self.closure_checked.lock().await;
                c.insert(trade.condition_id.clone(), true);
            }
            let mut t = self.trades.lock().await;
            t.remove(&market_key);
        }
        Ok(())
    }

    pub async fn reset_period(&self) {
        let mut c = self.closure_checked.lock().await;
        c.clear();
        crate::log_println!("Period reset");
    }

    pub async fn get_total_profit(&self) -> f64 {
        *self.total_profit.lock().await
    }

    pub async fn get_period_profit(&self) -> f64 {
        *self.period_profit.lock().await
    }

    /// Returns (Up matched batch prices, Down matched batch prices) for the price feed "bought" display.
    /// One price per batch (not per order), so one batch of 5 orders at $0.52 shows as $0.52 once.
    pub async fn get_matched_bought_prices(&self, market_key: &str) -> (Vec<f64>, Vec<f64>) {
        let batches = self.pending_batches.lock().await;
        let mut up_prices = Vec::new();
        let mut down_prices = Vec::new();
        if let Some(list) = batches.get(market_key) {
            for batch in list.iter() {
                let mut up_added = false;
                let mut down_added = false;
                for o in &batch.orders {
                    if !o.matched {
                        continue;
                    }
                    if o.side.eq_ignore_ascii_case("Up") {
                        if !up_added {
                            up_prices.push(o.price);
                            up_added = true;
                        }
                    } else {
                        if !down_added {
                            down_prices.push(o.price);
                            down_added = true;
                        }
                    }
                }
            }
        }
        (up_prices, down_prices)
    }
}
