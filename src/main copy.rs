use axum::{
    extract::State,
    response::Html,
    routing::get,
    Router,
    Json,
};
use std::env;
use std::fs::OpenOptions;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use hmac::{Hmac, Mac};
use ta::indicators::{BollingerBands, RelativeStrengthIndex};
use ta::Next;
use tokio::time::sleep;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;

// --- 🛠️ CONFIGURATION ---
const SIMULATION_MODE: bool = true;
const PAIR: &str = "B-BTC_USDT"; 
const TIMEFRAME: &str = "5m";
const TRADE_AMOUNT: f64 = 0.001;
const TRAILING_STOP_PCT: f64 = 0.005; 
const RSI_BUY: f64 = 30.0;
const RSI_SELL: f64 = 70.0;
const CSV_FILE: &str = "trades.csv";
const PORT: u16 = 3000; 

// --- 📊 SHARED APP STATE ---
#[derive(Clone, Serialize)]
struct DashboardData {
    price: f64,
    rsi: f64,
    bb_lower: f64,
    status: String,
    
    // NEW TRACKING FIELDS
    entry_price: f64,       // 0.0 if idle
    unrealized_pl: f64,     // % Profit/Loss currently
    realized_pl: f64,       // Total USDT banked
    
    logs: Vec<String>,      // Last 10 log lines for UI
}

type SharedState = Arc<RwLock<DashboardData>>;

// --- DATA STRUCTURES ---
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct Candle { close: f64 }

#[derive(Serialize)]
struct OrderPayload {
    side: String, order_type: String, market: String, 
    price_per_unit: Option<f64>, total_quantity: f64, timestamp: u128,
}

#[derive(Serialize)]
struct TradeRecord {
    timestamp: String, action: String, price: f64, 
    quantity: f64, mode: String, comment: String, pl: f64,
}

enum BotState {
    Idle,
    InPosition { entry_price: f64, highest_price: f64 },
}

// --- ⚡ ASYNC "ZERO LATENCY" LOGGER ---
// This spawns a detached task so the bot loop NEVER waits for disk I/O
fn log_async(state: &SharedState, action: &str, price: f64, comment: &str, pl: f64) {
    // 1. Update UI Logs immediately
    let log_msg = format!("{} | {} @ {:.2} ({})", 
        Utc::now().format("%H:%M:%S"), action.to_uppercase(), price, comment);
    
    {
        let mut data = state.write();
        data.logs.insert(0, log_msg); // Add to top
        if data.logs.len() > 20 { data.logs.pop(); } // Keep last 20
        
        // Update Realized P&L if it's a Sell
        if action == "sell" {
            data.realized_pl += pl;
        }
    }

    // 2. Write to CSV in Background
    let action = action.to_string();
    let comment = comment.to_string();
    let mode = if SIMULATION_MODE { "SIMULATION" } else { "REAL" }.to_string();

    tokio::spawn(async move {
        let file_exists = std::path::Path::new(CSV_FILE).exists();
        let file = OpenOptions::new().write(true).create(true).append(true).open(CSV_FILE);

        if let Ok(f) = file {
            let mut wtr = csv::WriterBuilder::new().has_headers(!file_exists).from_writer(f);
            let now: DateTime<Utc> = SystemTime::now().into();
            let record = TradeRecord {
                timestamp: now.format("%Y-%m-%d %H:%M:%S").to_string(),
                action, price, quantity: TRADE_AMOUNT, mode, comment, pl
            };
            let _ = wtr.serialize(record);
            let _ = wtr.flush();
        }
    });
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

async fn get_market_data(client: &reqwest::Client) -> Result<Vec<Candle>, reqwest::Error> {
    let url = "https://public.coindcx.com/market_data/candles";
    let params = [("pair", PAIR), ("interval", TIMEFRAME)];
    let resp = client.get(url).query(&params).send().await?.json::<Vec<Candle>>().await?;
    Ok(resp)
}

async fn execute_real_trade(client: &reqwest::Client, side: &str, price: f64) {
    if SIMULATION_MODE { return; }
    let (api_key, api_secret) = get_api_credentials();
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
    let market_code = if PAIR.starts_with("I-") { "BTCUSDT" } else { "BTCUSDT" };

    let payload = OrderPayload {
        side: side.to_string(), order_type: "limit_order".to_string(), market: market_code.to_string(), 
        price_per_unit: Some(price), total_quantity: TRADE_AMOUNT, timestamp,
    };

    let body_str = serde_json::to_string(&payload).unwrap();
    let signature = sign_payload(&body_str, &api_secret);
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert("X-AUTH-APIKEY", HeaderValue::from_str(&api_key).unwrap());
    headers.insert("X-AUTH-SIGNATURE", HeaderValue::from_str(&signature).unwrap());

    // let _ = client.post("https://api.coindcx.com/exchange/v1/orders/create").headers(headers).body(body_str).send().await;
    let _ = client; 
}

// --- 🚀 TRADING LOGIC TASK ---
async fn bot_logic(state: SharedState) {
    let client = reqwest::Client::new();
    let mut bb = BollingerBands::new(20, 2.0).unwrap();
    let mut rsi = RelativeStrengthIndex::new(14).unwrap();
    let mut bot_state = BotState::Idle;

    println!("🤖 Bot Engine Started...");

    loop {
        match get_market_data(&client).await {
            Ok(candles) => {
                if candles.is_empty() { sleep(Duration::from_secs(60)).await; continue; }

                for candle in &candles { bb.next(candle.close); rsi.next(candle.close); }
                
                let current_price = candles.last().unwrap().close;
                let current_rsi = rsi.next(current_price);
                let current_bb = bb.next(current_price);

                // --- UPDATE STATE & P&L ---
                {
                    let mut data = state.write();
                    data.price = current_price;
                    data.rsi = current_rsi;
                    data.bb_lower = current_bb.lower;
                    
                    // Calc Unrealized P&L if in position
                    if data.entry_price > 0.0 {
                        let diff = current_price - data.entry_price;
                        data.unrealized_pl = (diff / data.entry_price) * 100.0;
                    } else {
                        data.unrealized_pl = 0.0;
                    }
                }

                match bot_state {
                    BotState::Idle => {
                        // Aggressive Entry: RSI < 20 OR (RSI < 30 & Price < LowBB)
                        if (current_rsi < RSI_BUY && current_price < current_bb.lower) || (current_rsi < 20.0) {
                            println!("BUY SIGNAL @ ${}", current_price);
                            log_async(&state, "buy", current_price, "RSI+BB Entry", 0.0);
                            
                            {
                                let mut data = state.write();
                                data.status = "IN POSITION".to_string();
                                data.entry_price = current_price;
                            }
                            execute_real_trade(&client, "buy", current_price).await;
                            bot_state = BotState::InPosition { entry_price: current_price, highest_price: current_price };
                        } else {
                             state.write().status = "IDLE (Scanning)".to_string();
                        }
                    },
                    BotState::InPosition { entry_price, mut highest_price } => {
                        if current_price > highest_price { highest_price = current_price; }
                        let stop_price = highest_price * (1.0 - TRAILING_STOP_PCT);

                        // Calculate Profit for Log
                        let profit_amt = (current_price - entry_price) * TRADE_AMOUNT;

                        if current_price < stop_price {
                            println!("STOP LOSS @ ${}", current_price);
                            log_async(&state, "sell", current_price, "Trailing Stop", profit_amt);
                            
                            {
                                let mut data = state.write();
                                data.status = "IDLE".to_string();
                                data.entry_price = 0.0;
                            }
                            execute_real_trade(&client, "sell", current_price).await;
                            bot_state = BotState::Idle;
                        } else if current_rsi > RSI_SELL {
                            println!("PROFIT TAKE @ ${}", current_price);
                            log_async(&state, "sell", current_price, "RSI Overbought", profit_amt);
                            
                            {
                                let mut data = state.write();
                                data.status = "IDLE".to_string();
                                data.entry_price = 0.0;
                            }
                            execute_real_trade(&client, "sell", current_price).await;
                            bot_state = BotState::Idle;
                        } else {
                             // Just Update Status string for UI
                             state.write().status = format!("HOLDING (Stop: ${:.2})", stop_price);
                        }
                    }
                }
            }
            Err(e) => eprintln!("API Error: {}", e),
        }
        sleep(Duration::from_secs(60)).await;
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
        <title>CoinDCX Pro Dashboard</title>
        <style>
            body { font-family: 'Segoe UI', Tahoma, Geneva, Verdana, sans-serif; background: #121212; color: #e0e0e0; padding: 20px; text-align: center; }
            .container { max-width: 600px; margin: 0 auto; }
            
            /* CARDS */
            .card { background: #1e1e1e; padding: 20px; border-radius: 12px; margin-bottom: 15px; box-shadow: 0 4px 10px rgba(0,0,0,0.5); text-align: left; }
            .card-header { font-size: 0.9em; color: #888; text-transform: uppercase; margin-bottom: 10px; border-bottom: 1px solid #333; padding-bottom: 5px; }
            
            /* PRICE & STATUS */
            .big-price { font-size: 2.5em; font-weight: bold; color: #fff; text-align: center; }
            .status-badge { display: inline-block; padding: 5px 12px; border-radius: 20px; font-weight: bold; font-size: 0.8em; }
            .idle { background: #333; color: #aaa; }
            .active { background: #2196F3; color: white; animation: pulse 2s infinite; }
            
            /* GRID */
            .grid { display: grid; grid-template-columns: 1fr 1fr; gap: 15px; }
            .val-box { background: #252525; padding: 10px; border-radius: 8px; }
            .label { font-size: 0.8em; color: #777; }
            .value { font-size: 1.1em; font-weight: bold; margin-top: 2px; }
            
            /* P&L COLORS */
            .pos { color: #4CAF50; }
            .neg { color: #F44336; }
            
            /* LOGS */
            .log-box { background: #000; color: #00ff00; font-family: 'Courier New', monospace; font-size: 0.8em; height: 150px; overflow-y: auto; padding: 10px; border-radius: 8px; border: 1px solid #333; }
            .log-line { border-bottom: 1px solid #111; padding: 2px 0; }

            @keyframes pulse { 0% { opacity: 1; } 50% { opacity: 0.7; } 100% { opacity: 1; } }
        </style>
        <script>
            async function updateStats() {
                try {
                    let res = await fetch('/api/stats');
                    let data = await res.json();
                    
                    // 1. Header Stats
                    document.getElementById('price').innerText = "$" + data.price.toFixed(2);
                    let statusEl = document.getElementById('status');
                    statusEl.innerText = data.status;
                    statusEl.className = "status-box status-badge " + (data.status.includes("IDLE") ? "idle" : "active");

                    // 2. Position Card
                    document.getElementById('entry').innerText = data.entry_price > 0 ? "$" + data.entry_price.toFixed(2) : "--";
                    
                    let pl = data.unrealized_pl;
                    let plEl = document.getElementById('unrealized');
                    plEl.innerText = pl.toFixed(2) + "%";
                    plEl.className = "value " + (pl >= 0 ? "pos" : "neg");

                    document.getElementById('realized').innerText = "$" + data.realized_pl.toFixed(2);

                    // 3. Technicals
                    document.getElementById('rsi').innerText = data.rsi.toFixed(2);
                    document.getElementById('bb_low').innerText = "$" + data.bb_lower.toFixed(2);

                    // 4. Logs
                    let logHtml = "";
                    data.logs.forEach(line => {
                        logHtml += `<div class="log-line">> ${line}</div>`;
                    });
                    document.getElementById('logs').innerHTML = logHtml;

                } catch (e) { console.error(e); }
            }
            setInterval(updateStats, 2000);
        </script>
    </head>
    <body onload="updateStats()">
        <div class="container">
            <h1>🚀 Scalper Pi</h1>
            
            <div class="card" style="text-align: center;">
                <div id="status" class="status-badge idle">Connecting...</div>
                <div class="big-price" id="price">Loading...</div>
                <div style="font-size: 0.8em; color: #666; margin-top:5px;">BTC / USDT</div>
            </div>

            <div class="card">
                <div class="card-header">Position Details</div>
                <div class="grid">
                    <div class="val-box">
                        <div class="label">Entry Price</div>
                        <div class="value" id="entry">--</div>
                    </div>
                    <div class="val-box">
                        <div class="label">Unrealized P&L</div>
                        <div class="value" id="unrealized">0.00%</div>
                    </div>
                    <div class="val-box">
                        <div class="label">Total Realized Profit</div>
                        <div class="value pos" id="realized">$0.00</div>
                    </div>
                    <div class="val-box">
                        <div class="label">Trade Amount</div>
                        <div class="value">0.001 BTC</div>
                    </div>
                </div>
            </div>

            <div class="card">
                <div class="card-header">Indicators</div>
                <div class="grid">
                    <div class="val-box">
                        <div class="label">RSI (14)</div>
                        <div class="value" id="rsi">--</div>
                    </div>
                    <div class="val-box">
                        <div class="label">Buy Zone (BB Low)</div>
                        <div class="value" id="bb_low">--</div>
                    </div>
                </div>
            </div>

            <div class="card">
                <div class="card-header">System Logs</div>
                <div class="log-box" id="logs">
                    Waiting for data...
                </div>
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
        price: 0.0, rsi: 0.0, bb_lower: 0.0, status: "Starting...".to_string(),
        entry_price: 0.0, unrealized_pl: 0.0, realized_pl: 0.0, logs: vec![]
    }));

    let bot_state = shared_state.clone();
    tokio::spawn(async move {
        bot_logic(bot_state).await;
    });

    let app = Router::new()
        .route("/", get(dashboard_handler))
        .route("/api/stats", get(api_handler))
        .with_state(shared_state);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", PORT)).await.unwrap();
    println!("🌍 Dashboard running at http://0.0.0.0:{}", PORT);
    axum::serve(listener, app).await.unwrap();
}

// use axum::{
//     extract::State,
//     response::Html,
//     routing::get,
//     Router,
//     Json,
// };
// use std::env;
// use std::fs::OpenOptions;
// use std::sync::Arc;
// use std::time::{Duration, SystemTime, UNIX_EPOCH};
// use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
// use serde::{Deserialize, Serialize};
// use sha2::Sha256;
// use hmac::{Hmac, Mac};
// use ta::indicators::{BollingerBands, RelativeStrengthIndex};
// use ta::Next;
// use tokio::time::sleep;
// use chrono::{DateTime, Utc};
// use parking_lot::RwLock;

// // --- 🛠️ CONFIGURATION ---
// const SIMULATION_MODE: bool = true;
// const PAIR: &str = "B-BTC_USDT"; 
// const TIMEFRAME: &str = "5m";
// const TRADE_AMOUNT: f64 = 0.001;
// const TRAILING_STOP_PCT: f64 = 0.005; 
// const RSI_BUY: f64 = 30.0;
// const RSI_SELL: f64 = 70.0;
// const CSV_FILE: &str = "trades.csv";
// const PORT: u16 = 3000; 

// // --- 📊 SHARED APP STATE ---
// #[derive(Clone, Serialize)]
// struct DashboardData {
//     price: f64,
//     rsi: f64,
//     bb_lower: f64,
//     bb_upper: f64,
//     status: String,
//     last_trade: String,
//     mode: String,
// }

// type SharedState = Arc<RwLock<DashboardData>>;

// // --- DATA STRUCTURES ---
// #[allow(dead_code)]
// #[derive(Debug, Deserialize)]
// struct Candle { close: f64 }

// #[derive(Serialize)]
// struct OrderPayload {
//     side: String, order_type: String, market: String, 
//     price_per_unit: Option<f64>, total_quantity: f64, timestamp: u128,
// }

// #[derive(Serialize)]
// struct TradeRecord {
//     timestamp: String, action: String, price: f64, 
//     quantity: f64, mode: String, comment: String,
// }

// enum BotState {
//     Idle,
//     InPosition { entry_price: f64, highest_price: f64 },
// }

// // --- 💾 CSV HELPER ---
// fn log_trade_to_csv(action: &str, price: f64, comment: &str) {
//     let file_exists = std::path::Path::new(CSV_FILE).exists();
//     let file = OpenOptions::new().write(true).create(true).append(true).open(CSV_FILE).unwrap();
//     let mut wtr = csv::WriterBuilder::new().has_headers(!file_exists).from_writer(file);
//     let now: DateTime<Utc> = SystemTime::now().into();
//     let record = TradeRecord {
//         timestamp: now.format("%Y-%m-%d %H:%M:%S").to_string(),
//         action: action.to_string(), price, quantity: TRADE_AMOUNT,
//         mode: if SIMULATION_MODE { "SIMULATION".to_string() } else { "REAL".to_string() },
//         comment: comment.to_string(),
//     };
//     wtr.serialize(record).expect("Could not write to CSV");
//     wtr.flush().expect("Could not flush CSV");
// }

// // --- 🌐 API HELPERS ---
// fn get_api_credentials() -> (String, String) {
//     (env::var("COINDCX_API_KEY").unwrap_or("dummy".into()), env::var("COINDCX_SECRET_KEY").unwrap_or("dummy".into()))
// }

// fn sign_payload(payload: &str, secret: &str) -> String {
//     type HmacSha256 = Hmac<Sha256>;
//     let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("Invalid Key");
//     mac.update(payload.as_bytes());
//     hex::encode(mac.finalize().into_bytes())
// }

// async fn get_market_data(client: &reqwest::Client) -> Result<Vec<Candle>, reqwest::Error> {
//     let url = "https://public.coindcx.com/market_data/candles";
//     let params = [("pair", PAIR), ("interval", TIMEFRAME)];
//     let resp = client.get(url).query(&params).send().await?.json::<Vec<Candle>>().await?;
//     Ok(resp)
// }

// // Function to Execute Real Trade (Solves "unused" warnings)
// async fn execute_real_trade(client: &reqwest::Client, side: &str, price: f64) {
//     if SIMULATION_MODE { return; }

//     let (api_key, api_secret) = get_api_credentials();
//     let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
    
//     // Auto-detect market code for USDT
//     let market_code = if PAIR.starts_with("I-") { "BTCUSDT" } else { "BTCUSDT" };

//     let payload = OrderPayload {
//         side: side.to_string(),
//         order_type: "limit_order".to_string(),
//         market: market_code.to_string(), 
//         price_per_unit: Some(price),
//         total_quantity: TRADE_AMOUNT,
//         timestamp,
//     };

//     let body_str = serde_json::to_string(&payload).unwrap();
//     let signature = sign_payload(&body_str, &api_secret);

//     let mut headers = HeaderMap::new();
//     headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
//     headers.insert("X-AUTH-APIKEY", HeaderValue::from_str(&api_key).unwrap());
//     headers.insert("X-AUTH-SIGNATURE", HeaderValue::from_str(&signature).unwrap());

//     // Uncomment to enable real firing
//     // let _ = client.post("https://api.coindcx.com/exchange/v1/orders/create")
//     //     .headers(headers).body(body_str).send().await;
    
//     println!("(REAL) Order Sent to API: {} @ {}", side, price);
    
//     // Suppress unused variable warning for now
//     let _ = client; 
// }

// // --- 🚀 TRADING LOGIC TASK ---
// async fn bot_logic(state: SharedState) {
//     let client = reqwest::Client::new();
//     let mut bb = BollingerBands::new(20, 2.0).unwrap();
//     let mut rsi = RelativeStrengthIndex::new(14).unwrap();
//     let mut bot_state = BotState::Idle;

//     println!("🤖 Bot Engine Started...");

//     loop {
//         match get_market_data(&client).await {
//             Ok(candles) => {
//                 if candles.is_empty() {
//                     sleep(Duration::from_secs(60)).await;
//                     continue;
//                 }

//                 for candle in &candles {
//                     bb.next(candle.close);
//                     rsi.next(candle.close);
//                 }
                
//                 let current_price = candles.last().unwrap().close;
//                 let current_rsi = rsi.next(current_price);
//                 let current_bb = bb.next(current_price);

//                 // --- UPDATE DASHBOARD STATE ---
//                 {
//                     let mut data = state.write();
//                     data.price = current_price;
//                     data.rsi = current_rsi;
//                     data.bb_lower = current_bb.lower;
//                     data.bb_upper = current_bb.upper;
//                 }

//                 // --- STRATEGY ---
//                 match bot_state {
//                     BotState::Idle => {
//                         // Aggressive Entry: RSI < 20 OR (RSI < 30 & Price < LowBB)
//                         if (current_rsi < RSI_BUY && current_price < current_bb.lower) || (current_rsi < 20.0) {
//                             let msg = format!("BUY @ ${:.2} (RSI: {:.2})", current_price, current_rsi);
//                             println!("{}", msg);
//                             log_trade_to_csv("buy", current_price, "RSI+BB Entry");
                            
//                             state.write().status = "IN POSITION".to_string();
//                             state.write().last_trade = msg;

//                             // Call the real trade function (even if in sim mode, it handles the check)
//                             execute_real_trade(&client, "buy", current_price).await;
                            
//                             bot_state = BotState::InPosition { entry_price: current_price, highest_price: current_price };
//                         } else {
//                             state.write().status = "IDLE (Scanning)".to_string();
//                         }
//                     },
//                     BotState::InPosition { entry_price: _entry_price, mut highest_price } => {
//                         // Note: _entry_price prefix prevents "unused variable" warning
//                         if current_price > highest_price { highest_price = current_price; }
//                         let stop_price = highest_price * (1.0 - TRAILING_STOP_PCT);

//                         if current_price < stop_price {
//                             let msg = format!("STOP LOSS @ ${:.2}", current_price);
//                             println!("{}", msg);
//                             log_trade_to_csv("sell", current_price, "Trailing Stop");
//                             state.write().status = "IDLE".to_string();
//                             state.write().last_trade = msg;
                            
//                             execute_real_trade(&client, "sell", current_price).await;
                            
//                             bot_state = BotState::Idle;
//                         } else if current_rsi > RSI_SELL {
//                             let msg = format!("PROFIT TAKE @ ${:.2}", current_price);
//                             println!("{}", msg);
//                             log_trade_to_csv("sell", current_price, "RSI Overbought");
//                             state.write().status = "IDLE".to_string();
//                             state.write().last_trade = msg;

//                             execute_real_trade(&client, "sell", current_price).await;
                            
//                             bot_state = BotState::Idle;
//                         } else {
//                              state.write().status = format!("HOLDING (Stop: {:.2})", stop_price);
//                         }
//                     }
//                 }
//             }
//             Err(e) => eprintln!("API Error: {}", e),
//         }
//         sleep(Duration::from_secs(60)).await;
//     }
// }

// // --- 🖥️ DASHBOARD HANDLERS ---

// async fn dashboard_handler() -> Html<&'static str> {
//     Html(r#"
//     <!DOCTYPE html>
//     <html lang="en">
//     <head>
//         <meta charset="UTF-8">
//         <meta name="viewport" content="width=device-width, initial-scale=1.0">
//         <title>CoinDCX Scalper Pi</title>
//         <style>
//             body { font-family: 'Segoe UI', sans-serif; background: #1a1a1a; color: #fff; text-align: center; padding: 20px; }
//             .card { background: #2d2d2d; padding: 20px; border-radius: 12px; margin: 15px auto; max-width: 400px; box-shadow: 0 4px 6px rgba(0,0,0,0.3); }
//             h1 { color: #4CAF50; }
//             .price { font-size: 2.5em; font-weight: bold; }
//             .label { color: #888; font-size: 0.9em; }
//             .val { font-size: 1.2em; font-weight: bold; }
//             .grid { display: grid; grid-template-columns: 1fr 1fr; gap: 10px; text-align: left; }
//             .status-box { padding: 10px; border-radius: 8px; font-weight: bold; margin-top: 10px; }
//             .idle { background: #444; color: #ccc; }
//             .active { background: #2196F3; color: white; }
//         </style>
//         <script>
//             async function updateStats() {
//                 try {
//                     let res = await fetch('/api/stats');
//                     let data = await res.json();
//                     document.getElementById('price').innerText = "$" + data.price.toFixed(2);
//                     document.getElementById('rsi').innerText = data.rsi.toFixed(2);
//                     document.getElementById('bb_low').innerText = "$" + data.bb_lower.toFixed(2);
                    
//                     let statusEl = document.getElementById('status');
//                     statusEl.innerText = data.status;
//                     statusEl.className = "status-box " + (data.status.includes("IDLE") ? "idle" : "active");

//                     document.getElementById('last_trade').innerText = data.last_trade;
//                     document.getElementById('mode').innerText = data.mode;
//                 } catch (e) { console.error(e); }
//             }
//             setInterval(updateStats, 2000);
//         </script>
//     </head>
//     <body onload="updateStats()">
//         <h1>🤖 Crypto Scalper Pi</h1>
//         <div class="card">
//             <div class="label">Current Price</div>
//             <div class="price" id="price">Loading...</div>
//             <div class="status-box idle" id="status">Waiting...</div>
//         </div>

//         <div class="card grid">
//             <div>
//                 <div class="label">RSI (14)</div>
//                 <div class="val" id="rsi">--</div>
//             </div>
//             <div>
//                 <div class="label">Buy Target (BB Low)</div>
//                 <div class="val" id="bb_low">--</div>
//             </div>
//         </div>

//         <div class="card">
//             <div class="label">Last Action</div>
//             <div class="val" id="last_trade">None yet</div>
//             <div style="margin-top:10px; font-size:0.8em; color:#666;">Mode: <span id="mode">--</span></div>
//         </div>
//     </body>
//     </html>
//     "#)
// }

// async fn api_handler(State(state): State<SharedState>) -> Json<DashboardData> {
//     let data = state.read().clone();
//     Json(data)
// }

// // --- 🧠 MAIN ENTRY POINT ---
// #[tokio::main]
// async fn main() {
//     let shared_state = Arc::new(RwLock::new(DashboardData {
//         price: 0.0, rsi: 0.0, bb_lower: 0.0, bb_upper: 0.0,
//         status: "Starting...".to_string(), last_trade: "None".to_string(),
//         mode: if SIMULATION_MODE { "SIMULATION".to_string() } else { "REAL MONEY".to_string() },
//     }));

//     let bot_state = shared_state.clone();
//     tokio::spawn(async move {
//         bot_logic(bot_state).await;
//     });

//     let app = Router::new()
//         .route("/", get(dashboard_handler))
//         .route("/api/stats", get(api_handler))
//         .with_state(shared_state);

//     let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", PORT)).await.unwrap();
//     println!("🌍 Dashboard running at http://<RASPBERRY-PI-IP>:{}", PORT);
    
//     axum::serve(listener, app).await.unwrap();
// }