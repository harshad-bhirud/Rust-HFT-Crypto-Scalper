⚡ Rust HFT Crypto ScalperA high-performance, asynchronous high-frequency trading (HFT) bot written in Rust. Designed for low-latency execution on embedded devices (Raspberry Pi/VPS), it features a real-time web dashboard, embedded SQL storage with WAL mode, and robust risk management systems.Note: The current implementation is configured for the CoinDCX exchange API but represents a scalable architecture adaptable to Binance, Kraken, or generic REST/WebSocket APIs.🌟 Key Features🏗️ Core ArchitectureAsynchronous Engine: Built on tokio for non-blocking I/O, allowing simultaneous market data fetching, indicator calculation, and HTTP serving.Live OHLC Synthesis: Instead of relying on potentially delayed "closed" candles from the exchange, this bot aggregates real-time trade ticks into live 1-minute candles. This ensures indicators update every 5 seconds rather than once a minute.Embedded Database: Uses rusqlite with Write-Ahead Logging (WAL) enabled. This prevents "database locked" errors and allows external tools to query the DB while the bot is running.Auto-Pruning: Self-maintains the database by pruning records older than 60 minutes to ensure constant-time queries ($O(1)$) regardless of uptime.🖥️ Real-Time TelemetryWeb Dashboard: Integrated axum web server running on port 3000.Live Metrics: Displays Unrealized P&L, Realized Profit, Wallet Balance, and Indicator status.Zero-Latency Logging: Trade logs are written to CSV/SQLite via detached threads to prevent blocking the trading loop.🧠 Trading MethodologyThis bot implements a Mean Reversion Scalping Strategy, based on the statistical probability that price will revert to its average after an extreme deviation.1. Data PipelineHistorical Context: On startup, the bot fetches the last 50 candles (M1 timeframe) to "warm up" the indicators.Real-Time Synthesis: It continuously fetches the latest trade price (every 5s) and appends it to the historical data as the "current candle." This allows the RSI and Bollinger Bands to react during the candle formation, not just after it closes.2. IndicatorsBollinger Bands (BB): 20-period SMA with 2 standard deviations. Used to determine relative high/low price levels.Relative Strength Index (RSI): 14-period momentum oscillator. Used to measure the speed and change of price movements.3. Entry Logic (Buy Signals)The bot enters a LONG position if either of these conditions is met:Standard Mean Reversion: The asset is oversold (RSI < 30) AND the price has pierced below the Lower Bollinger Band (Price < BB_Lower).Crash Catch (Aggressive): The asset is deeply oversold (RSI < 20), indicating a panic dump. The bot buys immediately, ignoring Bollinger Bands, anticipating a "dead cat bounce."4. Exit Logic (Sell Signals)The bot exits the position based on dynamic risk management:Take Profit (Momentum): If momentum shifts to overbought (RSI > 70), the bot sells to lock in profits.Trailing Stop-Loss: Once a trade is entered, the bot tracks the Highest Price reached during the trade.A dynamic stop-loss is set at 0.5% below the Highest Price.If the price reverses by 0.5% from the peak, the bot sells immediately to protect gains or limit losses.🛠️ Tech StackComponentTechnologyDescriptionLanguageRust (2021)Memory safety and C++ level performance.RuntimetokioAsync runtime for managing concurrency.ServeraxumErgonomic and modular web framework.Databaserusqlite (Bundled)SQLite integration with zero external deps.HTTP ClientreqwestRobust HTTP client with connection pooling.Mathta crateTechnical analysis library.🚀 Installation & Setup1. PrerequisitesEnsure you have Rust and the necessary build tools installed.# Install Rust
curl --proto '=https' --tlsv1.2 -sSf [https://sh.rustup.rs](https://sh.rustup.rs) | sh

# Install SSL/Build dependencies (Ubuntu/Debian/Raspberry Pi)
sudo apt update
sudo apt install build-essential pkg-config libssl-dev
2. Clone the Repositorygit clone [https://github.com/yourusername/rust-hft-scalper.git](https://github.com/yourusername/rust-hft-scalper.git)
cd rust-hft-scalper
3. Configuration (.env)Security is paramount. Never hardcode your API keys. Create a .env file to manage credentials securely.Create the file in the project root:touch .env
Open it with a text editor:nano .env
Paste the following configuration (replace with your actual keys):# Exchange API Credentials (CoinDCX Example)
COINDCX_API_KEY="your_api_key_starts_with_..."
COINDCX_SECRET_KEY="your_secret_key_starts_with_..."

# Optional Logging Level (debug, info, warn, error)
RUST_LOG=info
Important: Ensure .env is in your .gitignore file to prevent accidental uploads to GitHub.4. Build the BinaryCompile the project in release mode for maximum optimization.cargo build --release
🏃 UsageRunning the BotOnce built, run the binary from the target directory. Ensure you load the environment variables first.# Load env vars
source .env 

# Run the bot
./target/release/coindcx_scalper
Systemd Service (Recommended for Deployment)To run the bot in the background and restart on boot:Edit the service file: sudo nano /etc/systemd/system/scalper.servicePaste configuration:[Unit]
Description=Rust Scalper Bot
After=network-online.target
Wants=network-online.target

[Service]
User=pi
WorkingDirectory=/home/pi/rust-hft-scalper
# Point to your compiled binary
ExecStart=/home/pi/rust-hft-scalper/target/release/coindcx_scalper
Restart=always
RestartSec=5

# Method 1: Load from .env file (Requires systemd version 229+)
# EnvironmentFile=/home/pi/rust-hft-scalper/.env

# Method 2: Define keys directly here
Environment="COINDCX_API_KEY=your_key"
Environment="COINDCX_SECRET_KEY=your_secret"

[Install]
WantedBy=multi-user.target
Enable and start:sudo systemctl daemon-reload
sudo systemctl enable --now scalper
📊 Dashboard & MonitoringAccess the dashboard via your browser:http://localhost:3000 (or http://<DEVICE_IP>:3000)Database InspectionSince the DB runs in WAL mode, you can inspect it while the bot runs without locking issues:sqlite3 bot_data.db
sqlite> .mode column
sqlite> .headers on
sqlite> SELECT * FROM trades ORDER BY id DESC LIMIT 5;
sqlite> .quit
⚙️ Logic CustomizationTo tune the strategy parameters, open src/main.rs and modify the constants at the top of the file.const SIMULATION_MODE: bool = true;   // Set to 'false' to trade real money
const PAIR: &str = "B-BTC_USDT";      // Trading Pair
const TIMEFRAME: &str = "1m";         // Candle size
const TRADE_CAPITAL: f64 = 10000.0;   // Position size in USDT
const RSI_BUY: f64 = 30.0;            // Buy Signal Threshold
const RSI_SELL: f64 = 70.0;           // Sell Signal Threshold
Note: You must rebuild the project (cargo build --release) for changes to take effect.⚠️ DisclaimerThis software is for educational and research purposes only. Cryptocurrency trading involves substantial risk of loss. The authors are not responsible for any financial losses incurred through the use of this software.Always test in SIMULATION_MODE for at least 24 hours.Start with small capital when moving to production.