# **⚡ Rust HFT Crypto Scalper**

A high-performance, asynchronous high-frequency trading (HFT) bot written in **Rust**. Designed for low-latency execution on embedded devices (Raspberry Pi/VPS), it features a real-time web dashboard, embedded SQL storage with WAL mode, and robust risk management systems.

**Note:** The current implementation is configured for the **CoinDCX** exchange API but represents a scalable architecture adaptable to Binance, Kraken, or generic REST/WebSocket APIs.

## **🌟 Key Features**

### **🏗️ Core Architecture**

* **Asynchronous Engine:** Built on tokio for non-blocking I/O, allowing simultaneous market data fetching, indicator calculation, and HTTP serving.  
* **Live OHLC Synthesis:** Instead of relying on potentially delayed "closed" candles from the exchange, this bot aggregates real-time trade ticks into live 1-minute candles. This ensures indicators update every 5 seconds rather than once a minute.  
* **Embedded Database:** Uses rusqlite with **Write-Ahead Logging (WAL)** enabled. This prevents "database locked" errors and allows external tools to query the DB while the bot is running.  
* **Auto-Pruning:** Self-maintains the database by pruning records older than 60 minutes to ensure constant-time queries (![][image1]) regardless of uptime.

### **🖥️ Real-Time Telemetry**

* **Web Dashboard:** Integrated axum web server running on port 3000\.  
* **Live Metrics:** Displays Unrealized P\&L, Realized Profit, Wallet Balance, and Indicator status.  
* **Zero-Latency Logging:** Trade logs are written to CSV/SQLite via detached threads to prevent blocking the trading loop.

## **🧠 Trading Methodology**

This bot implements a **Mean Reversion Scalping Strategy**, based on the statistical probability that price will revert to its average after an extreme deviation.

### **1\. Data Pipeline**

* **Historical Context:** On startup, the bot fetches the last 50 candles (M1 timeframe) to "warm up" the indicators.  
* **Real-Time Synthesis:** It continuously fetches the latest trade price (every 5s) and appends it to the historical data as the "current candle." This allows the RSI and Bollinger Bands to react *during* the candle formation, not just after it closes.

### **2\. Indicators**

* **Bollinger Bands (BB):** 20-period SMA with 2 standard deviations. Used to determine relative high/low price levels.  
* **Relative Strength Index (RSI):** 14-period momentum oscillator. Used to measure the speed and change of price movements.

### **3\. Entry Logic (Buy Signals)**

The bot enters a **LONG** position if either of these conditions is met:

* **Standard Mean Reversion:** The asset is oversold (RSI \< 30\) **AND** the price has pierced below the Lower Bollinger Band (Price \< BB\_Lower).  
* **Crash Catch (Aggressive):** The asset is deeply oversold (RSI \< 20), indicating a panic dump. The bot buys immediately, ignoring Bollinger Bands, anticipating a "dead cat bounce."

### **4\. Exit Logic (Sell Signals)**

The bot exits the position based on dynamic risk management:

* **Take Profit (Momentum):** If momentum shifts to overbought (RSI \> 70), the bot sells to lock in profits.  
* **Trailing Stop-Loss:** Once a trade is entered, the bot tracks the Highest Price reached during the trade.  
  * A dynamic stop-loss is set at **0.5% below the Highest Price**.  
  * If the price reverses by 0.5% from the peak, the bot sells immediately to protect gains or limit losses.

## **🛠️ Tech Stack**

| Component | Technology | Description |
| :---- | :---- | :---- |
| **Language** | Rust (2021) | Memory safety and C++ level performance. |
| **Runtime** | tokio | Async runtime for managing concurrency. |
| **Server** | axum | Ergonomic and modular web framework. |
| **Database** | rusqlite (Bundled) | SQLite integration with zero external deps. |
| **HTTP Client** | reqwest | Robust HTTP client with connection pooling. |
| **Math** | ta crate | Technical analysis library. |

## **🚀 Installation & Setup**

### **1\. Prerequisites**

Ensure you have Rust and the necessary build tools installed.

\# Install Rust  
curl \--proto '=https' \--tlsv1.2 \-sSf \[https://sh.rustup.rs\](https://sh.rustup.rs) | sh

\# Install SSL/Build dependencies (Ubuntu/Debian/Raspberry Pi)  
sudo apt update  
sudo apt install build-essential pkg-config libssl-dev

### **2\. Clone the Repository**

git clone \[https://github.com/yourusername/rust-hft-scalper.git\](https://github.com/yourusername/rust-hft-scalper.git)  
cd rust-hft-scalper

### **3\. Configuration (.env)**

Security is paramount. Never hardcode your API keys. Create a .env file to manage credentials securely.

1. Create the file in the project root:  
   touch .env

2. Open it with a text editor:  
   nano .env

3. Paste the following configuration (replace with your actual keys):  
   \# Exchange API Credentials (CoinDCX Example)  
   COINDCX\_API\_KEY="your\_api\_key\_starts\_with\_..."  
   COINDCX\_SECRET\_KEY="your\_secret\_key\_starts\_with\_..."

   \# Optional Logging Level (debug, info, warn, error)  
   RUST\_LOG=info

4. **Important:** Ensure .env is in your .gitignore file to prevent accidental uploads to GitHub.

### **4\. Build the Binary**

Compile the project in release mode for maximum optimization.

cargo build \--release

## **🏃 Usage**

### **Running the Bot**

Once built, run the binary from the target directory. Ensure you load the environment variables first.

\# Load env vars  
source .env 

\# Run the bot  
./target/release/coindcx\_scalper

### **Systemd Service (Recommended for Deployment)**

To run the bot in the background and restart on boot:

1. Edit the service file: sudo nano /etc/systemd/system/scalper.service  
2. Paste configuration:  
   \[Unit\]  
   Description=Rust Scalper Bot  
   After=network-online.target  
   Wants=network-online.target

   \[Service\]  
   User=pi  
   WorkingDirectory=/home/pi/rust-hft-scalper  
   \# Point to your compiled binary  
   ExecStart=/home/pi/rust-hft-scalper/target/release/coindcx\_scalper  
   Restart=always  
   RestartSec=5

   \# Method 1: Load from .env file (Requires systemd version 229+)  
   \# EnvironmentFile=/home/pi/rust-hft-scalper/.env

   \# Method 2: Define keys directly here  
   Environment="COINDCX\_API\_KEY=your\_key"  
   Environment="COINDCX\_SECRET\_KEY=your\_secret"

   \[Install\]  
   WantedBy=multi-user.target

3. Enable and start:  
   sudo systemctl daemon-reload  
   sudo systemctl enable \--now scalper

## **📊 Dashboard & Monitoring**

Access the dashboard via your browser:

http://localhost:3000 (or http://\<DEVICE\_IP\>:3000)

### **Database Inspection**

Since the DB runs in WAL mode, you can inspect it while the bot runs without locking issues:

sqlite3 bot\_data.db  
sqlite\> .mode column  
sqlite\> .headers on  
sqlite\> SELECT \* FROM trades ORDER BY id DESC LIMIT 5;  
sqlite\> .quit

## **⚙️ Logic Customization**

To tune the strategy parameters, open src/main.rs and modify the constants at the top of the file.

const SIMULATION\_MODE: bool \= true;   // Set to 'false' to trade real money  
const PAIR: \&str \= "B-BTC\_USDT";      // Trading Pair  
const TIMEFRAME: \&str \= "1m";         // Candle size  
const TRADE\_CAPITAL: f64 \= 10000.0;   // Position size in USDT  
const RSI\_BUY: f64 \= 30.0;            // Buy Signal Threshold  
const RSI\_SELL: f64 \= 70.0;           // Sell Signal Threshold

**Note:** You must rebuild the project (cargo build \--release) for changes to take effect.

## **⚠️ Disclaimer**

This software is for **educational and research purposes only**. Cryptocurrency trading involves substantial risk of loss. The authors are not responsible for any financial losses incurred through the use of this software.

* Always test in SIMULATION\_MODE for at least 24 hours.  
* Start with small capital when moving to production.

[image1]: <data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAACgAAAAZCAYAAABD2GxlAAACuElEQVR4Xu2WvWsUURTFZ1kVxe+PuCS7m9kvXAiCxYIiWNnYqEjSiGJtY2WhWEsgIIKECP4DdoKFSCxSBNIELEQwCEELJawgyIIQC0Xi77hvkjfX2dlRtxDMgcvu3HPufWfum3m7QbCJfwytVmtrqVQ6YPMpyI2Oju63ycyoVCr7wjAc5mvecha1Wm0v2sfEhOVSkEN/TbWWSAVF94nPRJs7fM/nd2K60WjssVrBmZvlhm5xmbO8UK/Xyz3qZXI2k0m2ZwfiOyw0VSwWD0Z5rs+Q72B2GU3DrxHI34Bf9Gt8qCfxFd05ywmqVY+gx80FNC4heu4mlbhFelacZs1QmsDrarVaMPk8UzvsHpN51fUyqFr1IGaCJJMQT1yDe4mCLrageWQNMtEiuWk/Z9HPoKAexApRtZzINaKdtH0eNKmH0jKV7VGyXC6fJzfuCy0yGhyXRv1ihBuvDCaP12FoaGhXtJBvkOtJehzztRZZDMK10KyqX4wIu6P9gKAWIwzcpHQj61scme53lmUxODIycgjNkrTryUKhsJPEHLHQbDZ3b8h/gbZ3xhlciZKRQX36YossBr0dmk9Mpi2iZxNN2xm8HeXd+beYViv8scFg48FPNai325l7paMjyg9ygtpBNAvWoBa/SXJJz0CM8BB2z8cOL8NxPx8ZTDgDY8hiEH6YeEfMWe6nAYqXWSj00nnyl4hVtvKIl48B/jq1p2zeB5oXzuAV/aGwvKAeaL5xQly1nMizkB+JL3x/wOck8cblLlu9D/iTSU39Y8lG0iTdTnY4LY5aLkIeUYvFLhIXiEqQci5GcC/KU/9s/F2oVj1Y/5n+D1j+r0HzT2l33g+qVQ8Nx3IDgV4eFnjLAicslwWqtS/gwMEiE8RL86L1hfRhn9/ygYGFTof2tzQFY2Nj29DftflN/Df4ATkZ1EFjDSKlAAAAAElFTkSuQmCC>