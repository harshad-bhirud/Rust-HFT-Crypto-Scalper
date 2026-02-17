use axum::{
    extract::State,
    response::Html,
    routing::get,
    Router,
    Json,
};
use std::env;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE, CACHE_CONTROL, PRAGMA};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use hmac::{Hmac, Mac};
use ta::indicators::{BollingerBands, RelativeStrengthIndex};
use ta::Next;
use tokio::time::sleep;
use chrono::{DateTime, Utc}; 
use parking_lot::RwLock;
use rusqlite::{params, Connection, Result as SqlResult};

// --- 🛠️ CONFIGURATION ---
const SIMULATION_MODE: bool = true; 
const PAIR: &str = "B-BTC_USDT"; 
const TIMEFRAME: &str = "1m"; // 1 Minute candles
const TRADE_CAPITAL: f64 = 10000.0; // Trade size in USDT
const TRAILING_STOP_PCT: f64 = 0.005; // 0.5%
const RSI_BUY: f64 = 30.0;
const RSI_SELL: f64 = 70.0;
const DB_FILE: &str = "bot_data.db";
const PORT: u16 = 3000; 

// --- 📊 SHARED APP STATE ---
#[derive(Clone, Serialize)]
struct DashboardData {
    price: f64,
    rsi: f64,
    bb_lower: f64,
    bb_upper: f64, 
    status: String,
    entry_price: f64,       
    unrealized_pl: f64,     
    realized_pl: f64, 
    wallet_usdt: f64,       
    wallet_btc: f64,        
    logs: Vec<String>,      
}

type SharedState = Arc<RwLock<DashboardData>>;

// --- DATA STRUCTURES ---
#[derive(Debug, Deserialize, Clone)]
struct Candle { 
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    time: i64 
}

// Helper to handle "123.45" (string) or 123.45 (number) from API
fn f64_from_str_or_num<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Val {
        String(String),
        Number(f64),
    }

    match Val::deserialize(deserializer)? {
        Val::String(s) => s.parse::<f64>().map_err(serde::de::Error::custom),
        Val::Number(n) => Ok(n),
    }
}

#[derive(Debug, Deserialize)]
struct TradeTick {
    #[serde(alias = "p", deserialize_with = "f64_from_str_or_num")]
    price: f64,
}

#[derive(Serialize)]
struct OrderPayload {
    side: String, order_type: String, market: String, 
    price_per_unit: Option<f64>, total_quantity: f64, timestamp: u128,
}

#[derive(Debug, Deserialize)]
struct Balance {
    currency: String,
    balance: String,
}

enum BotState {
    Idle,
    InPosition { entry_price: f64, highest_price: f64, quantity: f64 },
}

// --- 🗄️ DATABASE MANAGER ---
struct DbManager;

impl DbManager {
    fn connect() -> SqlResult<Connection> {
        Connection::open(DB_FILE)
    }

    fn init() -> SqlResult<()> {
        let conn = Self::connect()?;
        
        // 🛑 FIX: Enable WAL mode for concurrent access
        conn.pragma_update(None, "journal_mode", "WAL")?;

        // 🛑 FIX: Drop old tables to ensure schema matches code (Handles bb_upper addition)
        conn.execute("DROP TABLE IF EXISTS candles", [])?;
        conn.execute("DROP TABLE IF EXISTS trades", [])?;
        
        // Candles Table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS candles (
                time INTEGER PRIMARY KEY,
                open REAL, high REAL, low REAL, close REAL,
                rsi REAL, bb_lower REAL, bb_upper REAL
            )",
            [],
        )?;

        // Trades Table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS trades (
                id INTEGER PRIMARY KEY,
                action TEXT, price REAL, quantity REAL, profit REAL, timestamp TEXT
            )",
            [],
        )?;
        println!("🗄️ Database Initialized & Schema Reset (WAL Mode)");
        Ok(())
    }

    fn save_candle(candle: &Candle, rsi: f64, bb_lower: f64, bb_upper: f64) -> SqlResult<()> {
        let conn = Self::connect()?;
        conn.execute(
            "INSERT OR REPLACE INTO candles (time, open, high, low, close, rsi, bb_lower, bb_upper)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![candle.time, candle.open, candle.high, candle.low, candle.close, rsi, bb_lower, bb_upper],
        )?;
        Ok(())
    }

    fn get_recent_candles(limit: usize) -> SqlResult<Vec<Candle>> {
        let conn = Self::connect()?;
        let mut stmt = conn.prepare("SELECT time, open, high, low, close FROM candles ORDER BY time DESC LIMIT ?1")?;
        let candle_iter = stmt.query_map(params![limit], |row| {
            Ok(Candle {
                time: row.get(0)?,
                open: row.get(1)?,
                high: row.get(2)?,
                low: row.get(3)?,
                close: row.get(4)?,
            })
        })?;

        let mut candles = Vec::new();
        for candle in candle_iter { candles.push(candle?); }
        candles.reverse(); 
        Ok(candles)
    }

    fn log_trade(action: &str, price: f64, qty: f64, profit: f64) -> SqlResult<()> {
        let conn = Self::connect()?;
        let time_str = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO trades (action, price, quantity, profit, timestamp) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![action, price, qty, profit, time_str],
        )?;
        Ok(())
    }

    fn prune_old_data() -> SqlResult<()> {
        let conn = Self::connect()?;
        let threshold = Utc::now().timestamp_millis() - (60 * 60 * 1000); 
        conn.execute("DELETE FROM candles WHERE time < ?1", params![threshold])?;
        Ok(())
    }
}

// --- 🌐 API HELPERS ---
fn get_api_credentials() -> (String, String) {
    (env::var("COINDCX_API_KEY").unwrap_or("dummy".into()), env::var("COINDCX_SECRET_KEY").unwrap_or("dummy".into()))
}

fn sign_payload(payload: &str, secret: &str) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("Invalid Key");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn add_log(state: &SharedState, msg: String) {
    let mut data = state.write();
    let time_str = Utc::now().format("%H:%M:%S").to_string();
    println!("{} | {}", time_str, msg); 
    data.logs.insert(0, format!("{} | {}", time_str, msg));
    if data.logs.len() > 30 { data.logs.pop(); }
}

async fn fetch_historical_candles(client: &reqwest::Client) -> Result<Vec<Candle>, reqwest::Error> {
    let url = "https://public.coindcx.com/market_data/candles";
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis().to_string();
    let params = [("pair", PAIR), ("interval", TIMEFRAME), ("_t", &timestamp)];

    let resp = client.get(url)
        .query(&params)
        .header(CACHE_CONTROL, "no-cache") 
        .header(PRAGMA, "no-cache")
        .send()
        .await?
        .json::<Vec<Candle>>()
        .await?;
    Ok(resp)
}

async fn get_latest_price(client: &reqwest::Client) -> Result<Option<f64>, reqwest::Error> {
    let url = "https://public.coindcx.com/market_data/trade_history";
    let params = [("pair", PAIR), ("limit", "1")]; 
    let resp = client.get(url).query(&params).header(CACHE_CONTROL, "no-cache").send().await?.json::<Vec<TradeTick>>().await?;
    
    if let Some(trade) = resp.first() {
        Ok(Some(trade.price))
    } else {
        Ok(None)
    }
}

async fn fetch_wallet_balance(client: &reqwest::Client, state: &SharedState) {
    if SIMULATION_MODE {
        let mut data = state.write();
        data.wallet_usdt = 10500.0; 
        data.wallet_btc = 0.05;
        return;
    }

    let (api_key, api_secret) = get_api_credentials();
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
    let payload = serde_json::json!({ "timestamp": timestamp });
    let body_str = payload.to_string();
    let signature = sign_payload(&body_str, &api_secret);

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert("X-AUTH-APIKEY", HeaderValue::from_str(&api_key).unwrap());
    headers.insert("X-AUTH-SIGNATURE", HeaderValue::from_str(&signature).unwrap());

    if let Ok(res) = client.post("https://api.coindcx.com/exchange/v1/users/balances").headers(headers).body(body_str).send().await {
        if let Ok(balances) = res.json::<Vec<Balance>>().await {
            let mut usdt = 0.0;
            let mut btc = 0.0;
            for b in balances {
                if b.currency == "USDT" { usdt = b.balance.parse().unwrap_or(0.0); }
                if b.currency == "BTC" { btc = b.balance.parse().unwrap_or(0.0); }
            }
            let mut data = state.write();
            data.wallet_usdt = usdt;
            data.wallet_btc = btc;
        }
    }
}

async fn execute_trade(client: &reqwest::Client, side: &str, price: f64, qty: f64) {
    if SIMULATION_MODE { 
        println!("(SIMULATION) {} {} BTC @ ${}", side, qty, price);
        let _ = DbManager::log_trade(side, price, qty, 0.0); 
        return; 
    }
    
    let (api_key, api_secret) = get_api_credentials();
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
    let market_code = "BTCUSDT"; 

    let payload = OrderPayload {
        side: side.to_string(), order_type: "limit_order".to_string(), market: market_code.to_string(), 
        price_per_unit: Some(price), total_quantity: qty, timestamp,
    };

    let body_str = serde_json::to_string(&payload).unwrap();
    let signature = sign_payload(&body_str, &api_secret);
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert("X-AUTH-APIKEY", HeaderValue::from_str(&api_key).unwrap());
    headers.insert("X-AUTH-SIGNATURE", HeaderValue::from_str(&signature).unwrap());

    // 🛑 FIX: Real execution enabled (when SIMULATION_MODE is false)
    let res = client.post("https://api.coindcx.com/exchange/v1/orders/create").headers(headers).body(body_str).send().await;
    match res {
        Ok(r) => println!("(REAL) API Response: {:?}", r.status()),
        Err(e) => eprintln!("(REAL) API Error: {}", e),
    }
}

// --- 🧠 CORE LOGIC ---
async fn bot_logic(state: SharedState) {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap();
    
    // 1. Init DB & History (Drops old table to fix schema)
    let _ = DbManager::init();
    match fetch_historical_candles(&client).await {
        Ok(candles) => {
            let mut bb = BollingerBands::new(20, 2.0).unwrap();
            let mut rsi = RelativeStrengthIndex::new(14).unwrap();

            for candle in candles.iter().rev() {
                let bb_out = bb.next(candle.close);
                let rsi_val = rsi.next(candle.close);
                let _ = DbManager::save_candle(candle, rsi_val, bb_out.lower, bb_out.upper); 
            }
            add_log(&state, format!("Synced {} candles to DB", candles.len()));
        },
        Err(e) => eprintln!("History Sync Failed: {}", e),
    }

    let mut bot_state = BotState::Idle;
    let mut last_prune = SystemTime::now();
    let mut last_wallet = SystemTime::now();
    
    let mut current_candle = Candle { open: 0.0, high: 0.0, low: 0.0, close: 0.0, time: 0 };

    loop {
        if last_prune.elapsed().unwrap() > Duration::from_secs(300) {
            let _ = DbManager::prune_old_data();
            add_log(&state, "Pruned old DB data".to_string());
            last_prune = SystemTime::now();
        }

        if last_wallet.elapsed().unwrap() > Duration::from_secs(60) {
            fetch_wallet_balance(&client, &state).await;
            last_wallet = SystemTime::now();
        }

        match get_latest_price(&client).await {
            Ok(Some(price)) => {
                let now_ts = Utc::now().timestamp_millis();
                let candle_start_ts = (now_ts / 60000) * 60000;

                if current_candle.time != candle_start_ts {
                    current_candle = Candle { open: price, high: price, low: price, close: price, time: candle_start_ts };
                } else {
                    current_candle.close = price;
                    if price > current_candle.high { current_candle.high = price; }
                    if price < current_candle.low { current_candle.low = price; }
                }

                let _ = DbManager::save_candle(&current_candle, 0.0, 0.0, 0.0);

                let history = DbManager::get_recent_candles(50).unwrap_or_default();
                let mut bb = BollingerBands::new(20, 2.0).unwrap();
                let mut rsi = RelativeStrengthIndex::new(14).unwrap();
                let mut cur_rsi = 0.0;
                let mut cur_bb_low = 0.0;
                let mut cur_bb_high = 0.0; 

                for c in &history {
                    let bb_out = bb.next(c.close);
                    cur_bb_low = bb_out.lower;
                    cur_bb_high = bb_out.upper; 
                    cur_rsi = rsi.next(c.close);
                }

                let _ = DbManager::save_candle(&current_candle, cur_rsi, cur_bb_low, cur_bb_high);

                {
                    let mut data = state.write();
                    data.price = price;
                    data.rsi = cur_rsi;
                    data.bb_lower = cur_bb_low;
                    data.bb_upper = cur_bb_high;
                    if let BotState::InPosition { entry_price, .. } = bot_state {
                        let diff = price - entry_price;
                        data.unrealized_pl = (diff / entry_price) * 100.0;
                    } else {
                        data.unrealized_pl = 0.0;
                    }
                }

                match bot_state {
                    BotState::Idle => {
                        if (cur_rsi < RSI_BUY && price < cur_bb_low) || (cur_rsi < 20.0) {
                            let qty = TRADE_CAPITAL / price;
                            add_log(&state, format!("BUY SIGNAL @ ${:.2}", price));
                            
                            {
                                let mut data = state.write();
                                data.status = "IN POSITION".to_string();
                                data.entry_price = price;
                            }
                            execute_trade(&client, "buy", price, qty).await;
                            bot_state = BotState::InPosition { entry_price: price, highest_price: price, quantity: qty };
                        } else {
                             state.write().status = "IDLE (Scanning)".to_string();
                        }
                    },
                    BotState::InPosition { entry_price, mut highest_price, quantity } => {
                        if price > highest_price { highest_price = price; }
                        let stop_price = highest_price * (1.0 - TRAILING_STOP_PCT);
                        let profit_amt = (price - entry_price) * quantity;

                        if price < stop_price {
                            add_log(&state, format!("STOP LOSS @ ${:.2}", price));
                            let _ = DbManager::log_trade("sell", price, quantity, profit_amt);
                            {
                                let mut data = state.write();
                                data.status = "IDLE".to_string();
                                data.entry_price = 0.0;
                                data.realized_pl += profit_amt;
                            }
                            execute_trade(&client, "sell", price, quantity).await;
                            bot_state = BotState::Idle;
                        } else if cur_rsi > RSI_SELL {
                            add_log(&state, format!("PROFIT TAKE @ ${:.2}", price));
                            let _ = DbManager::log_trade("sell", price, quantity, profit_amt);
                            {
                                let mut data = state.write();
                                data.status = "IDLE".to_string();
                                data.entry_price = 0.0;
                                data.realized_pl += profit_amt;
                            }
                            execute_trade(&client, "sell", price, quantity).await;
                            bot_state = BotState::Idle;
                        } else {
                             state.write().status = "HOLDING".to_string();
                             bot_state = BotState::InPosition { entry_price, highest_price, quantity };
                        }
                    }
                }
            },
            Ok(None) => eprintln!("No trades found in recent history"),
            Err(e) => eprintln!("Tick Error: {}", e),
        }
        sleep(Duration::from_secs(5)).await;
    }
}

// --- 🖥️ DASHBOARD HANDLERS ---
async fn dashboard_handler() -> Html<&'static str> {
    Html(r#"
    <!DOCTYPE html>
    <html lang="en">
    <head>
        <meta charset="UTF-8">
        <meta name="viewport" content="width=device-width, initial-scale=1.0">
        <title>CoinDCX Scalper v2</title>
        <style>
            body { font-family: 'Segoe UI', sans-serif; background: #121212; color: #e0e0e0; padding: 20px; text-align: center; }
            .container { max-width: 600px; margin: 0 auto; }
            .card { background: #1e1e1e; padding: 20px; border-radius: 12px; margin-bottom: 15px; box-shadow: 0 4px 10px rgba(0,0,0,0.5); text-align: left; }
            .big-price { font-size: 2.5em; font-weight: bold; color: #fff; text-align: center; }
            .status-badge { display: inline-block; padding: 5px 12px; border-radius: 20px; font-weight: bold; font-size: 0.8em; }
            .idle { background: #333; color: #aaa; }
            .active { background: #2196F3; color: white; animation: pulse 2s infinite; }
            .grid { display: grid; grid-template-columns: 1fr 1fr; gap: 15px; }
            .val-box { background: #252525; padding: 10px; border-radius: 8px; }
            .label { font-size: 0.8em; color: #777; }
            .value { font-size: 1.1em; font-weight: bold; margin-top: 2px; }
            .pos { color: #4CAF50; } .neg { color: #F44336; }
            .log-box { background: #000; color: #00ff00; font-family: 'Courier New', monospace; font-size: 0.8em; height: 150px; overflow-y: auto; padding: 10px; border-radius: 8px; border: 1px solid #333; }
            @keyframes pulse { 0% { opacity: 1; } 50% { opacity: 0.7; } 100% { opacity: 1; } }
        </style>
        <script>
            // 🛑 SAFETY: Check element existence to prevent crashes on old cached HTML
            function safeSetText(id, val) {
                const el = document.getElementById(id);
                if(el) el.innerText = val;
            }
            function safeSetClass(id, val) {
                const el = document.getElementById(id);
                if(el) el.className = val;
            }

            async function updateStats() {
                try {
                    // FIX: Use absolute URL to prevent "Request cannot be constructed from a URL that includes credentials" error
                    const url = window.location.origin + '/api/stats?t=' + Date.now();
                    let res = await fetch(url);
                    let data = await res.json();
                    
                    safeSetText('price', "$" + data.price.toFixed(2));
                    safeSetText('status', data.status);
                    safeSetClass('status', "status-badge " + (data.status.includes("IDLE") ? "idle" : "active"));
                    
                    safeSetText('entry', data.entry_price > 0 ? "$" + data.entry_price.toFixed(2) : "--");
                    
                    const pl = data.unrealized_pl;
                    safeSetText('unrealized', pl.toFixed(2) + "%");
                    safeSetClass('unrealized', "value " + (pl >= 0 ? "pos" : "neg"));
                    
                    safeSetText('realized', "$" + data.realized_pl.toFixed(2));
                    safeSetText('rsi', data.rsi.toFixed(2));
                    
                    safeSetText('bb_low', "$" + data.bb_lower.toFixed(2));
                    safeSetText('bb_high', "$" + data.bb_upper.toFixed(2));
                    
                    safeSetText('usdt', "$" + data.wallet_usdt.toFixed(2));
                    safeSetText('btc', data.wallet_btc.toFixed(5) + " BTC");
                    
                    let logHtml = "";
                    data.logs.forEach(line => { logHtml += `<div>> ${line}</div>`; });
                    const logsEl = document.getElementById('logs');
                    if(logsEl) logsEl.innerHTML = logHtml;
                    
                } catch (e) { console.error("Update Error:", e); }
            }
            setInterval(updateStats, 2000);
        </script>
    </head>
    <body onload="updateStats()">
        <div class="container">
            <h1>🚀 Scalper Pi v2</h1>
            <div class="card" style="text-align: center;">
                <div id="status" class="status-badge idle">Connecting...</div>
                <div class="big-price" id="price">Loading...</div>
            </div>
            
            <div class="card">
                <div class="grid">
                    <div class="val-box"><div class="label">Entry</div><div class="value" id="entry">--</div></div>
                    <div class="val-box"><div class="label">P&L</div><div class="value" id="unrealized">0.00%</div></div>
                    <div class="val-box"><div class="label">Realized Profit</div><div class="value pos" id="realized">$0.00</div></div>
                    <div class="val-box"><div class="label">RSI</div><div class="value" id="rsi">--</div></div>
                    <div class="val-box"><div class="label">BB Low</div><div class="value" id="bb_low">--</div></div>
                    <div class="val-box"><div class="label">BB High</div><div class="value" id="bb_high">--</div></div>
                </div>
            </div>

            <div class="card">
                <div style="font-size:0.9em; color:#888; margin-bottom: 5px;">Wallet Balance</div>
                <div class="grid">
                    <div class="val-box"><div class="label">USDT Available</div><div class="value" id="usdt">--</div></div>
                    <div class="val-box"><div class="label">BTC Available</div><div class="value" id="btc">--</div></div>
                </div>
            </div>

            <div class="card">
                <div class="log-box" id="logs">Waiting for data...</div>
            </div>
        </div>
    </body>
    </html>
    "#)
}

async fn api_handler(State(state): State<SharedState>) -> Json<DashboardData> {
    let data = state.read().clone();
    Json(data)
}

#[tokio::main]
async fn main() {
    let shared_state = Arc::new(RwLock::new(DashboardData {
        price: 0.0, rsi: 0.0, bb_lower: 0.0, bb_upper: 0.0, status: "Starting...".to_string(),
        entry_price: 0.0, unrealized_pl: 0.0, realized_pl: 0.0, 
        wallet_usdt: 0.0, wallet_btc: 0.0, logs: vec![]
    }));

    let state_shutdown = shared_state.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.unwrap();
        println!("\n🚨 SHUTDOWN: Checking open positions...");
        let (in_pos, price, qty) = {
            let d = state_shutdown.read();
            (d.entry_price > 0.0, d.price, 0.001)
        };
        if in_pos {
            println!("💥 EMERGENCY SELL: Closing at {}", price);
            let client = reqwest::Client::new();
            execute_trade(&client, "sell", price, qty).await;
        }
        std::process::exit(0);
    });

    let bot_state = shared_state.clone();
    tokio::spawn(async move {
        bot_logic(bot_state).await;
    });

    let app = Router::new().route("/", get(dashboard_handler)).route("/api/stats", get(api_handler)).with_state(shared_state);
    
    let listener = loop {
        match tokio::net::TcpListener::bind(format!("0.0.0.0:{}", PORT)).await {
            Ok(l) => { println!("🌍 Dashboard: http://0.0.0.0:{}", PORT); break l; },
            Err(_) => { eprintln!("⚠️ Port busy, retrying..."); sleep(Duration::from_secs(5)).await; }
        }
    };
    axum::serve(listener, app).await.unwrap();
}