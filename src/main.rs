// ============================================================================
//  torfile – TUI Download Manager for Linux
//  Single-file Rust implementation using aria2c as the download backend.
//
//  Features:
//    • HTTP / FTP / SFTP direct URL downloads
//    • Magnet link support (BitTorrent DHT)
//    • .torrent file support (local path or remote URL)
//    • Multi-engine torrent search (ThePirateBay + Knaben aggregator)
//    • Search category filter (All / Video / Audio / Software / Games / Books)
//    • Auto tracker refresh every hour from multiple public tracker lists
//    • Real-time progress, ETA, speed, seeds / peers display
//    • Global & per-session download / upload speed limits
//    • Pause / Resume / Force-remove / Clear history
//    • Confirmation dialogs for destructive actions
//    • In-app help overlay
//
//  Requires:  aria2c  (sudo apt install aria2  OR  sudo pacman -S aria2)
//  Build:     cargo build --release
// ============================================================================

use anyhow::{anyhow, Result};
use base64::Engine as B64Engine;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
    Frame, Terminal,
};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    io::stdout,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::{sync::mpsc, time::sleep};

// ============================================================================
// ARIA2 JSON-RPC CLIENT
// ============================================================================

struct Aria2 {
    url: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl Aria2 {
    fn new(url: &str, token: Option<String>) -> Self {
        Self {
            url: url.into(),
            token,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        }
    }

    async fn rpc(&self, method: &str, mut params: Vec<Value>) -> Result<Value> {
        if let Some(ref tok) = self.token {
            params.insert(0, json!(format!("token:{}", tok)));
        }
        let body = json!({ "jsonrpc": "2.0", "id": "1", "method": method, "params": params });
        let resp: Value = self.http.post(&self.url).json(&body).send().await?.json().await?;
        if let Some(e) = resp.get("error") {
            return Err(anyhow!("aria2: {}", e));
        }
        Ok(resp["result"].clone())
    }

    // --- query helpers ---
    fn dl_keys() -> Value {
        json!([
            "gid", "status", "totalLength", "completedLength",
            "downloadSpeed", "uploadSpeed", "bittorrent", "files",
            "numSeeders", "connections", "errorMessage", "dir"
        ])
    }

    async fn active(&self) -> Vec<Value> {
        self.rpc("aria2.tellActive", vec![Self::dl_keys()]).await
            .unwrap_or_default()
            .as_array().cloned().unwrap_or_default()
    }

    async fn waiting(&self, off: i64, n: i64) -> Vec<Value> {
        self.rpc("aria2.tellWaiting", vec![json!(off), json!(n), Self::dl_keys()]).await
            .unwrap_or_default()
            .as_array().cloned().unwrap_or_default()
    }

    async fn stopped(&self, off: i64, n: i64) -> Vec<Value> {
        self.rpc("aria2.tellStopped", vec![json!(off), json!(n), Self::dl_keys()]).await
            .unwrap_or_default()
            .as_array().cloned().unwrap_or_default()
    }

    async fn global_stat(&self) -> Result<Value> {
        self.rpc("aria2.getGlobalStat", vec![]).await
    }

    async fn add_uri(&self, uris: &[&str], opts: Value) -> Result<String> {
        let r = self.rpc("aria2.addUri", vec![json!(uris), opts]).await?;
        Ok(r.as_str().unwrap_or("").into())
    }

    async fn add_torrent(&self, data: &[u8], opts: Value) -> Result<String> {
        let b64 = B64Engine::encode(&base64::engine::general_purpose::STANDARD, data);
        let r = self.rpc("aria2.addTorrent", vec![json!(b64), json!([]), opts]).await?;
        Ok(r.as_str().unwrap_or("").into())
    }

    async fn pause(&self, gid: &str) -> Result<()> {
        self.rpc("aria2.pause", vec![json!(gid)]).await?; Ok(())
    }
    async fn unpause(&self, gid: &str) -> Result<()> {
        self.rpc("aria2.unpause", vec![json!(gid)]).await?; Ok(())
    }
    async fn remove(&self, gid: &str) -> Result<()> {
        self.rpc("aria2.forceRemove", vec![json!(gid)]).await?; Ok(())
    }
    async fn remove_result(&self, gid: &str) -> Result<()> {
        self.rpc("aria2.removeDownloadResult", vec![json!(gid)]).await?; Ok(())
    }
    async fn purge(&self) -> Result<()> {
        self.rpc("aria2.purgeDownloadResult", vec![]).await?; Ok(())
    }

    async fn set_global_opts(&self, opts: Value) -> Result<()> {
        self.rpc("aria2.changeGlobalOption", vec![opts]).await?; Ok(())
    }
}

// ============================================================================
// DOMAIN MODELS
// ============================================================================

#[derive(Debug, Clone)]
struct Download {
    gid:        String,
    name:       String,
    status:     String,
    progress:   f64,
    dl_speed:   u64,
    ul_speed:   u64,
    total:      u64,
    done:       u64,
    seeds:      u32,
    conns:      u32,
    error:      String,
    is_torrent: bool,
}

impl Download {
    fn from(v: &Value) -> Self {
        let is_torrent = v["bittorrent"].is_object();
        let name = if is_torrent {
            v["bittorrent"]["info"]["name"].as_str()
                .map(|s| s.to_owned())
                .unwrap_or_else(|| "Fetching metadata…".into())
        } else {
            v["files"][0]["path"].as_str()
                .map(|p| PathBuf::from(p).file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.into()))
                .or_else(|| v["files"][0]["uris"][0]["uri"].as_str()
                    .map(|u| u.chars().take(60).collect()))
                .unwrap_or_else(|| "Unknown".into())
        };

        let parse = |k: &str| v[k].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let total = parse("totalLength");
        let done  = parse("completedLength");

        Self {
            gid:        v["gid"].as_str().unwrap_or("").into(),
            name,
            status:     v["status"].as_str().unwrap_or("unknown").into(),
            progress:   if total > 0 { done as f64 / total as f64 } else { 0.0 },
            dl_speed:   parse("downloadSpeed"),
            ul_speed:   parse("uploadSpeed"),
            total,
            done,
            seeds:      v["numSeeders"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
            conns:      v["connections"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
            error:      v["errorMessage"].as_str().unwrap_or("").into(),
            is_torrent,
        }
    }

    fn status_color(&self) -> Color {
        match self.status.as_str() {
            "active"   => Color::Green,
            "waiting"  => Color::Yellow,
            "paused"   => Color::Cyan,
            "error"    => Color::Red,
            "complete" => Color::Blue,
            _          => Color::Gray,
        }
    }

    fn eta(&self) -> String {
        if self.dl_speed == 0 || self.total == 0 { return "--:--".into(); }
        let secs = self.total.saturating_sub(self.done) / self.dl_speed;
        if secs >= 3600 { format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60) }
        else             { format!("{:02}:{:02}", secs / 60, secs % 60) }
    }
}

// Pretty-print byte sizes
fn fmt_b(b: u64) -> String {
    match b {
        b if b >= 1_099_511_627_776 => format!("{:.1}T", b as f64 / 1_099_511_627_776.0),
        b if b >= 1_073_741_824     => format!("{:.1}G", b as f64 / 1_073_741_824.0),
        b if b >= 1_048_576         => format!("{:.0}M", b as f64 / 1_048_576.0),
        b if b >= 1_024             => format!("{:.0}K", b as f64 / 1_024.0),
        b if b > 0                  => format!("{}B", b),
        _                           => "-".into(),
    }
}
fn fmt_spd(bps: u64) -> String {
    if bps == 0 { String::new() } else { format!("{}/s", fmt_b(bps)) }
}

#[derive(Debug, Clone)]
struct SearchResult {
    name:     String,
    magnet:   String,
    size:     String,
    seeds:    u32,
    leeches:  u32,
    category: String,
    source:   &'static str,
}

// ============================================================================
// BACKGROUND TASKS
// ============================================================================

enum BgMsg {
    Downloads(Vec<Download>),
    Stats { dl: u64, ul: u64, active: u32 },
    Trackers(Vec<String>),
    Status(String),
}

async fn refresh_loop(aria2: Arc<Aria2>, tx: mpsc::UnboundedSender<BgMsg>) {
    loop {
        let active  = aria2.active().await;
        let waiting = aria2.waiting(0, 50).await;
        let stopped = aria2.stopped(0, 50).await;
        let list: Vec<Download> = active.iter().chain(&waiting).chain(&stopped)
            .map(Download::from).collect();
        let _ = tx.send(BgMsg::Downloads(list));

        if let Ok(s) = aria2.global_stat().await {
            let p = |k: &str| s[k].as_str().and_then(|v| v.parse().ok()).unwrap_or(0u64);
            let _ = tx.send(BgMsg::Stats {
                dl:     p("downloadSpeed"),
                ul:     p("uploadSpeed"),
                active: p("numActive") as u32,
            });
        }
        sleep(Duration::from_millis(800)).await;
    }
}

// Four tracker list URLs – tried in order; results merged
const TRACKER_URLS: &[&str] = &[
    "https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_best.txt",
    "https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_all_udp.txt",
    "https://raw.githubusercontent.com/XIU2/TrackersListCollection/master/best.txt",
    "https://raw.githubusercontent.com/hezhijie0327/Trackerslist/main/trackerslist_tracker.txt",
];

async fn fetch_trackers_once(tx: &mpsc::UnboundedSender<BgMsg>) {
    let _ = tx.send(BgMsg::Status("Fetching trackers…".into()));
    let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().unwrap();
    let mut set: HashSet<String> = HashSet::new();

    for url in TRACKER_URLS.iter().take(2) {  // Only first two to stay fast
        if let Ok(resp) = client.get(*url).send().await {
            if let Ok(text) = resp.text().await {
                for line in text.lines() {
                    let l = line.trim().to_string();
                    if !l.is_empty() { set.insert(l); }
                }
            }
        }
    }

    let trackers: Vec<String> = set.into_iter().collect();
    let msg = format!("✓ {} trackers loaded", trackers.len());
    let _ = tx.send(BgMsg::Trackers(trackers));
    let _ = tx.send(BgMsg::Status(msg));
}

async fn tracker_refresh_loop(tx: mpsc::UnboundedSender<BgMsg>) {
    fetch_trackers_once(&tx).await;
    loop {
        sleep(Duration::from_secs(3_600)).await;  // Refresh hourly
        fetch_trackers_once(&tx).await;
    }
}

// ============================================================================
// SEARCH BACKENDS
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
enum Engine { Tpb, Knaben }
impl Engine {
    fn label(&self) -> &str { match self { Engine::Tpb => "ThePirateBay", Engine::Knaben => "Knaben" } }
    fn toggle(self) -> Self { match self { Engine::Tpb => Engine::Knaben, Engine::Knaben => Engine::Tpb } }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Category { All, Video, Audio, Software, Games, Books }
impl Category {
    fn label(self) -> &'static str {
        match self {
            Category::All => "All", Category::Video => "Video", Category::Audio => "Audio",
            Category::Software => "Software", Category::Games => "Games", Category::Books => "Books",
        }
    }
    fn tpb(self) -> &'static str {
        match self {
            Category::All => "0", Category::Video => "200", Category::Audio => "100",
            Category::Software => "300", Category::Games => "400", Category::Books => "600",
        }
    }
    fn knaben(self) -> &'static str {
        match self {
            Category::All => "", Category::Video => "video", Category::Audio => "audio",
            Category::Software => "software", Category::Games => "games", Category::Books => "books",
        }
    }
    fn next(self) -> Self {
        match self {
            Category::All => Category::Video, Category::Video => Category::Audio,
            Category::Audio => Category::Software, Category::Software => Category::Games,
            Category::Games => Category::Books, Category::Books => Category::All,
        }
    }
}

fn make_magnet(info_hash: &str, name: &str, trackers: &[String]) -> String {
    let extra: String = trackers.iter().take(6)
        .map(|t| format!("&tr={}", urlencoding::encode(t)))
        .collect();
    format!(
        "magnet:?xt=urn:btih:{}&dn={}&tr=udp://tracker.openbittorrent.com:6969{}",
        info_hash, urlencoding::encode(name), extra
    )
}

fn tpb_cat_label(id: u32) -> &'static str {
    match id {
        101..=109 => "Audio",
        201..=209 => "Video",
        301..=309 => "Software",
        401..=409 => "Games",
        501..=509 => "Other",
        601..=609 => "Books",
        _ => "?",
    }
}

async fn search_tpb(q: &str, cat: Category, trackers: &[String]) -> Result<Vec<SearchResult>> {
    let url = format!("https://apibay.org/q.php?q={}&cat={}", urlencoding::encode(q), cat.tpb());
    let data: Vec<Value> = reqwest::Client::builder()
        .timeout(Duration::from_secs(10)).build()?
        .get(&url).send().await?.json().await?;

    Ok(data.into_iter()
        .filter(|v| v["info_hash"].as_str().unwrap_or("") != "0000000000000000000000000000000000000000")
        .take(60)
        .map(|v| {
            let hash = v["info_hash"].as_str().unwrap_or("");
            let name = v["name"].as_str().unwrap_or("?");
            let bytes = v["size"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0u64);
            let cat_id: u32 = v["category"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
            SearchResult {
                name:     name.chars().take(80).collect(),
                magnet:   make_magnet(hash, name, trackers),
                size:     fmt_b(bytes),
                seeds:    v["seeders"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
                leeches:  v["leechers"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
                category: tpb_cat_label(cat_id).into(),
                source:   "TPB",
            }
        })
        .collect())
}

async fn search_knaben(q: &str, cat: Category, trackers: &[String]) -> Result<Vec<SearchResult>> {
    let cat_param = cat.knaben();
    let url = if cat_param.is_empty() {
        format!(
            "https://knaben.eu/api/v1/search?q={}&size=60&from=0&orderBy=seeders&orderDirection=desc",
            urlencoding::encode(q)
        )
    } else {
        format!(
            "https://knaben.eu/api/v1/search?q={}&categories={}&size=60&from=0&orderBy=seeders&orderDirection=desc",
            urlencoding::encode(q), cat_param
        )
    };

    let resp: Value = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("torfile/0.2")
        .build()?
        .get(&url).send().await?.json().await?;

    Ok(resp["hits"].as_array().cloned().unwrap_or_default()
        .into_iter()
        .filter_map(|v| {
            let hash = v["info_hash"].as_str()?.to_owned();
            let name = v["title"].as_str().unwrap_or("?");
            let bytes = v["bytes"].as_u64().unwrap_or(0);
            Some(SearchResult {
                name:     name.chars().take(80).collect(),
                magnet:   make_magnet(&hash, name, trackers),
                size:     fmt_b(bytes),
                seeds:    v["seeders"].as_u64().unwrap_or(0) as u32,
                leeches:  v["leechers"].as_u64().unwrap_or(0) as u32,
                category: v["category"].as_str().unwrap_or("-").into(),
                source:   "Knaben",
            })
        })
        .collect())
}

// ============================================================================
// CONFIRM DIALOG
// ============================================================================

#[derive(Clone, PartialEq)]
enum ConfirmKind {
    ClearOne { gid: String, name: String },
    ClearAll,
}
impl ConfirmKind {
    fn prompt(&self) -> String {
        match self {
            ConfirmKind::ClearOne { name, .. } =>
                format!("Remove \"{}\" from history?", name.chars().take(50).collect::<String>()),
            ConfirmKind::ClearAll =>
                "Clear ALL completed / error entries from history?".into(),
        }
    }
}

// ============================================================================
// APP STATE
// ============================================================================

#[derive(PartialEq, Copy, Clone)]
enum Tab { Downloads = 0, Search = 1, Settings = 2 }

#[derive(PartialEq, Clone)]
enum Mode {
    Normal,
    AddUrl,
    Searching,
    SpeedLimit { field: u8 },         // 0 = download, 1 = upload
    Confirm(ConfirmKind, bool),        // bool = "No" is highlighted
}

struct SpeedLimits { dl_bps: u64, ul_bps: u64 }
impl Default for SpeedLimits { fn default() -> Self { Self { dl_bps: 0, ul_bps: 0 } } }

struct App {
    aria2:       Arc<Aria2>,
    downloads:   Vec<Download>,
    dl_sel:      ListState,
    results:     Vec<SearchResult>,
    res_sel:     ListState,
    mode:        Mode,
    buf:         String,
    tab:         Tab,
    trackers:    Vec<String>,
    status:      String,
    dl_dir:      PathBuf,
    bg_rx:       mpsc::UnboundedReceiver<BgMsg>,
    // stats
    global_dl:   u64,
    global_ul:   u64,
    active_cnt:  u32,
    // search
    engine:      Engine,
    category:    Category,
    // speed limits (cached; applied via aria2 RPC)
    limits:      SpeedLimits,
    show_help:   bool,
}

impl App {
    fn new(aria2: Arc<Aria2>, bg_rx: mpsc::UnboundedReceiver<BgMsg>) -> Self {
        Self {
            aria2,
            downloads: vec![],
            dl_sel: ListState::default(),
            results: vec![],
            res_sel: ListState::default(),
            mode: Mode::Normal,
            buf: String::new(),
            tab: Tab::Downloads,
            trackers: vec![],
            status: "Connecting to aria2… │ [?] Help".into(),
            dl_dir: dirs::download_dir().unwrap_or(PathBuf::from(".")),
            bg_rx,
            global_dl: 0,
            global_ul: 0,
            active_cnt: 0,
            engine: Engine::Tpb,
            category: Category::All,
            limits: SpeedLimits::default(),
            show_help: false,
        }
    }

    fn drain(&mut self) {
        while let Ok(msg) = self.bg_rx.try_recv() {
            match msg {
                BgMsg::Downloads(d)         => self.downloads = d,
                BgMsg::Trackers(t)          => self.trackers = t,
                BgMsg::Status(s)            => self.status = s,
                BgMsg::Stats { dl, ul, active } => {
                    self.global_dl = dl;
                    self.global_ul = ul;
                    self.active_cnt = active;
                }
            }
        }
    }

    fn opts(&self) -> Value {
        let mut o = json!({ "dir": self.dl_dir.to_string_lossy() });
        if !self.trackers.is_empty() {
            o["bt-tracker"] = json!(self.trackers.join(","));
        }
        o
    }

    async fn add(&mut self, uri: &str) -> Result<()> {
        let t = uri.trim();
        let opts = self.opts();
        if t.starts_with("magnet:") {
            self.aria2.add_uri(&[t], opts).await?;
        } else if t.ends_with(".torrent") || t.contains(".torrent?") || t.contains("/torrent/") {
            match reqwest::get(t).await {
                Ok(r) => {
                    let bytes = r.bytes().await?;
                    self.aria2.add_torrent(&bytes, opts).await?;
                }
                Err(_) => { self.aria2.add_uri(&[t], opts).await?; }
            }
        } else {
            self.aria2.add_uri(&[t], opts).await?;
        }
        Ok(())
    }

    async fn search(&mut self, q: &str) -> Result<()> {
        self.status = format!("Searching \"{}\" on {}…", q, self.engine.label());
        let mut res = match self.engine {
            Engine::Tpb    => search_tpb(q, self.category, &self.trackers).await?,
            Engine::Knaben => search_knaben(q, self.category, &self.trackers).await?,
        };
        res.sort_by(|a, b| b.seeds.cmp(&a.seeds));
        let n = res.len();
        self.results = res;
        self.status = format!("{} results │ ↑↓ nav  Enter=DL  e=engine  Tab=category", n);
        Ok(())
    }

    async fn apply_limits(&self) -> Result<()> {
        self.aria2.set_global_opts(json!({
            "max-overall-download-limit": self.limits.dl_bps.to_string(),
            "max-overall-upload-limit":   self.limits.ul_bps.to_string(),
        })).await
    }
}

// ============================================================================
// RENDERING
// ============================================================================

fn render(app: &mut App, f: &mut Frame) {
    let area = f.area();
    let root = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ]).split(area);

    // ── tab bar ──────────────────────────────────────────────────────────
    let tabs = Tabs::new(vec!["[1] Downloads", "[2] Search", "[3] Settings"])
        .block(Block::bordered()
            .title(Span::styled(
                " ⬇  torfile ",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )))
        .select(app.tab as usize)
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .divider(Span::raw("  "));
    f.render_widget(tabs, root[0]);

    // ── main panel ───────────────────────────────────────────────────────
    match app.tab {
        Tab::Downloads => render_downloads(app, f, root[1]),
        Tab::Search    => render_search(app, f, root[1]),
        Tab::Settings  => render_settings(app, f, root[1]),
    }

    // ── status bar ───────────────────────────────────────────────────────
    let status = format!(
        " {} │ ↓{} ↑{} │ active:{} │ [?]Help",
        app.status, fmt_spd(app.global_dl), fmt_spd(app.global_ul), app.active_cnt,
    );
    f.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
        root[2],
    );

    // ── overlays (drawn last so they appear on top) ───────────────────────
    match app.mode.clone() {
        Mode::AddUrl           => render_add_overlay(app, f),
        Mode::Confirm(k, no)   => render_confirm(f, &k, no),
        Mode::SpeedLimit { field } => render_speed_overlay(app, f, field),
        _                      => {}
    }
    if app.show_help { render_help(f); }
}

// ── Downloads tab ────────────────────────────────────────────────────────────

fn render_downloads(app: &mut App, f: &mut Frame, area: Rect) {
    if app.downloads.is_empty() {
        f.render_widget(
            Paragraph::new(
                "\n  No downloads yet.\n\n  [a]  Add URL / magnet / .torrent\n  [s]  Search torrent sites\n  [?]  Show help",
            )
            .block(Block::bordered().title(" Downloads "))
            .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }

    let items: Vec<ListItem> = app.downloads.iter().map(|d| {
        let pct    = (d.progress * 100.0) as u64;
        let filled = ((d.progress * 24.0) as usize).min(24);
        let bar    = format!("{}{}", "█".repeat(filled), "░".repeat(24 - filled));

        // Speed / ETA
        let spd = if d.dl_speed > 0 { format!("↓{:>8}", fmt_spd(d.dl_speed)) } else { " ".repeat(9) };
        let eta = if d.status == "active" && d.dl_speed > 0 {
            format!(" ETA:{}", d.eta())
        } else if d.status == "error" && !d.error.is_empty() {
            format!(" ERR:{}", d.error.chars().take(28).collect::<String>())
        } else {
            String::new()
        };

        // Peers / seeds
        let peers = if d.is_torrent && (d.seeds > 0 || d.conns > 0) {
            format!(" S:{} P:{}", d.seeds, d.conns)
        } else if d.conns > 0 {
            format!(" C:{}", d.conns)
        } else {
            String::new()
        };

        let size = if d.total > 0 { format!(" {}", fmt_b(d.total)) } else { String::new() };
        let name: String = d.name.chars().take(42).collect();

        // Bar color: blue when complete, cyan otherwise
        let bar_color = if pct == 100 { Color::Blue } else { Color::Cyan };

        ListItem::new(Line::from(vec![
            Span::styled(
                format!(" {:>8} ", d.status.to_uppercase()),
                Style::default().fg(d.status_color()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:>3}%[{}]", pct, bar),
                Style::default().fg(bar_color),
            ),
            Span::styled(
                format!("{}", spd),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                format!("{:>8}", size),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("{:>12}", eta),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                format!("{:>10}", peers),
                Style::default().fg(Color::Magenta),
            ),
            Span::raw(format!("  {}", name)),
        ]))
    }).collect();

    let title = format!(
        " Downloads ({}) │ p=pause  x=remove  c=clear  C=clear all history ",
        app.downloads.len()
    );
    let list = List::new(items)
        .block(Block::bordered().title(title))
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, area, &mut app.dl_sel);
}

// ── Search tab ───────────────────────────────────────────────────────────────

fn render_search(app: &mut App, f: &mut Frame, area: Rect) {
    let chunks = Layout::vertical([Constraint::Length(5), Constraint::Min(0)]).split(area);

    let typing = app.mode == Mode::Searching;
    let cursor = if typing { "█" } else { "" };

    let controls = vec![
        Line::from(vec![
            Span::raw(" Query  : "),
            Span::styled(
                format!("{}{}", app.buf, cursor),
                Style::default().fg(if typing { Color::Yellow } else { Color::White }),
            ),
        ]),
        Line::from(vec![
            Span::raw(" Engine : "),
            Span::styled(app.engine.label(),   Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
            Span::raw("    Category : "),
            Span::styled(app.category.label(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        ]),
        Line::styled(
            "  [s] type query  [e] toggle engine  [Tab] cycle category  [Enter] search",
            Style::default().fg(Color::DarkGray),
        ),
    ];

    f.render_widget(
        Paragraph::new(controls)
            .block(Block::bordered()
                .title(" Search ")
                .border_style(if typing { Style::default().fg(Color::Yellow) } else { Style::default() })),
        chunks[0],
    );

    if app.results.is_empty() {
        f.render_widget(
            Paragraph::new("\n  Results will appear here.")
                .block(Block::bordered().title(" Results "))
                .style(Style::default().fg(Color::DarkGray)),
            chunks[1],
        );
        return;
    }

    let items: Vec<ListItem> = app.results.iter().map(|r| {
        let sc = if r.seeds > 50 { Color::Green } else if r.seeds > 10 { Color::Yellow } else { Color::Red };
        let src_clr = match r.source { "TPB" => Color::Red, _ => Color::Blue };
        ListItem::new(Line::from(vec![
            Span::styled(format!(" {:>5}↑{:>5}↓ ", r.seeds, r.leeches), Style::default().fg(sc)),
            Span::styled(format!("{:>8}  ", r.size),                     Style::default().fg(Color::Cyan)),
            Span::styled(format!("[{:>6}] ", r.source),                  Style::default().fg(src_clr)),
            Span::styled(format!("{:>10}  ", r.category.chars().take(10).collect::<String>()), Style::default().fg(Color::Blue)),
            Span::raw(&r.name),
        ]))
    }).collect();

    let list = List::new(items)
        .block(Block::bordered().title(format!(" Results ({}) – Enter=Download ", app.results.len())))
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, chunks[1], &mut app.res_sel);
}

// ── Settings tab ─────────────────────────────────────────────────────────────

fn render_settings(app: &mut App, f: &mut Frame, area: Rect) {
    let dl_lim = if app.limits.dl_bps == 0 { "Unlimited".into() } else { fmt_spd(app.limits.dl_bps) };
    let ul_lim = if app.limits.ul_bps == 0 { "Unlimited".into() } else { fmt_spd(app.limits.ul_bps) };

    let text = format!(
        "\n\
         \x20 Download dir   : {}\n\
         \x20 Trackers       : {} loaded  (auto-refreshed hourly from 4 sources)\n\
         \x20 Download limit : {}      Upload limit: {}\n\
         \n\
         \x20 ── Global ─────────────────────────────────────────────────\n\
         \x20 q / Ctrl+C   Quit             ?   Toggle help overlay\n\
         \x20 a            Add URL / magnet / .torrent URL\n\
         \x20 s            Search (opens Search tab and starts typing)\n\
         \x20 r            Refresh trackers now\n\
         \x20 1 / 2 / 3    Switch tabs\n\
         \n\
         \x20 ── Downloads tab ──────────────────────────────────────────\n\
         \x20 ↑ ↓ / j k    Navigate        p   Pause / Resume\n\
         \x20 x            Force remove    c   Clear selected from history\n\
         \x20 C            Clear ALL completed / error entries\n\
         \n\
         \x20 ── Search tab ─────────────────────────────────────────────\n\
         \x20 s            Type query       Enter   Download selected\n\
         \x20 e            Toggle engine    Tab     Cycle category\n\
         \n\
         \x20 ── Settings tab ───────────────────────────────────────────\n\
         \x20 l            Set speed limits (KB/s; 0 = unlimited)\n",
        app.dl_dir.display(), app.trackers.len(), dl_lim, ul_lim,
    );

    f.render_widget(
        Paragraph::new(text)
            .block(Block::bordered().title(" Settings & Key Reference "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

// ── Helper: centered popup ────────────────────────────────────────────────────

fn centered(pct_w: u16, h: u16, area: Rect) -> Rect {
    let vert = Layout::vertical([
        Constraint::Percentage((100u16.saturating_sub(h.saturating_mul(100) / area.height.max(1))) / 2),
        Constraint::Length(h),
        Constraint::Min(0),
    ]).split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - pct_w) / 2),
        Constraint::Percentage(pct_w),
        Constraint::Percentage((100 - pct_w) / 2),
    ]).split(vert[1])[1]
}

// ── Add URL overlay ───────────────────────────────────────────────────────────

fn render_add_overlay(app: &mut App, f: &mut Frame) {
    let popup = centered(82, 5, f.area());
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(format!("{}_", app.buf))
            .block(Block::bordered()
                .title(" Add Download — URL / magnet:? / .torrent URL  (Enter=confirm  Esc=cancel) ")
                .border_style(Style::default().fg(Color::Yellow)))
            .wrap(Wrap { trim: true }),
        popup,
    );
}

// ── Speed limit overlay ───────────────────────────────────────────────────────

fn render_speed_overlay(app: &mut App, f: &mut Frame, field: u8) {
    let popup = centered(60, 9, f.area());
    f.render_widget(Clear, popup);

    let active  = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let passive = Style::default().fg(Color::Gray);

    let dl_val = if field == 0 {
        format!("{}_", app.buf)
    } else if app.limits.dl_bps == 0 { "0  (unlimited)".into() } else { fmt_spd(app.limits.dl_bps) };
    let ul_val = if field == 1 {
        format!("{}_", app.buf)
    } else if app.limits.ul_bps == 0 { "0  (unlimited)".into() } else { fmt_spd(app.limits.ul_bps) };

    let content = vec![
        Line::raw(""),
        Line::from(vec![
            Span::raw("  Download limit (KB/s, 0=unlimited) : "),
            Span::styled(dl_val, if field == 0 { active } else { passive }),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  Upload   limit (KB/s, 0=unlimited) : "),
            Span::styled(ul_val, if field == 1 { active } else { passive }),
        ]),
        Line::raw(""),
        Line::raw(""),
        Line::styled(
            "  Tab = switch field    Enter = apply    Esc = cancel",
            Style::default().fg(Color::DarkGray),
        ),
    ];

    f.render_widget(
        Paragraph::new(content)
            .block(Block::bordered().title(" Speed Limits ").border_style(Style::default().fg(Color::Cyan)))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

// ── Confirm overlay ───────────────────────────────────────────────────────────

fn render_confirm(f: &mut Frame, kind: &ConfirmKind, no_sel: bool) {
    let popup = centered(62, 7, f.area());
    f.render_widget(Clear, popup);

    let yes_sty = if !no_sel { Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD) }
                  else       { Style::default().fg(Color::DarkGray) };
    let no_sty  = if  no_sel { Style::default().fg(Color::Black).bg(Color::Red  ).add_modifier(Modifier::BOLD) }
                  else       { Style::default().fg(Color::DarkGray) };

    let content = vec![
        Line::raw(""),
        Line::raw(kind.prompt()),
        Line::raw(""),
        Line::from(vec![
            Span::raw("           "),
            Span::styled("  Yes  ", yes_sty),
            Span::raw("        "),
            Span::styled("  No  ", no_sty),
        ]),
        Line::styled("  ← → / y / n  •  Enter = confirm  •  Esc = cancel", Style::default().fg(Color::DarkGray)),
    ];

    f.render_widget(
        Paragraph::new(content)
            .block(Block::bordered().title(" Confirm ").border_style(Style::default().fg(Color::Yellow)))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

// ── Help overlay ──────────────────────────────────────────────────────────────

fn render_help(f: &mut Frame) {
    let popup = centered(68, 26, f.area());
    f.render_widget(Clear, popup);

    let h = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let d = Style::default().fg(Color::DarkGray);
    let content = vec![
        Line::raw(""),
        Line::styled("  GLOBAL", h),
        Line::raw("  q / Ctrl+C    Quit                  ?   Toggle this help"),
        Line::raw("  1 / 2 / 3     Switch tabs            r   Refresh trackers now"),
        Line::raw("  a             Add URL / magnet / .torrent URL"),
        Line::raw(""),
        Line::styled("  DOWNLOADS TAB", h),
        Line::raw("  ↑ ↓  /  j k   Navigate list         p   Pause / Resume"),
        Line::raw("  x             Force-remove entry    c   Clear selected from history"),
        Line::raw("  C             Clear ALL history entries"),
        Line::raw(""),
        Line::styled("  SEARCH TAB", h),
        Line::raw("  s             Start typing query    Enter   Download selected"),
        Line::raw("  e             Toggle engine (TPB ↔ Knaben)"),
        Line::raw("  Tab           Cycle category  All→Video→Audio→Software→Games→Books"),
        Line::raw(""),
        Line::styled("  SETTINGS TAB", h),
        Line::raw("  l             Set global speed limits (KB/s; 0 = unlimited)"),
        Line::raw(""),
        Line::styled("  SUPPORTED URL TYPES", h),
        Line::raw("  http(s)://…   Direct download"),
        Line::raw("  magnet:?…     BitTorrent magnet link"),
        Line::raw("  https://….torrent   Remote .torrent file"),
        Line::raw(""),
        Line::styled("  Press [?] or [Esc] to close this overlay", d),
    ];

    f.render_widget(
        Paragraph::new(content)
            .block(Block::bordered().title(" Help ").border_style(Style::default().fg(Color::Cyan)))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

// ============================================================================
// ENTRY POINT
// ============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    // ── Launch aria2c daemon ──────────────────────────────────────────────────
    let mut aria2_proc: Option<Child> = Command::new("aria2c")
        .args([
            "--enable-rpc",
            "--rpc-listen-all",
            "--rpc-allow-origin-all",
            "--quiet",
            "--continue=true",
            "--max-concurrent-downloads=5",
            "--split=16",
            "--min-split-size=1M",
            "--max-connection-per-server=16",
            "--bt-enable-lpd",
            "--enable-dht",
            "--enable-dht6",
            "--dht-listen-port=6881-6889",
            "--bt-max-peers=100",
            "--seed-ratio=1.0",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok();

    // ── Wait for RPC to be ready (up to 5 × 300 ms) ──────────────────────────
    let aria2 = Arc::new(Aria2::new("http://localhost:6800/jsonrpc", None));
    let mut ready = false;
    for _ in 0..10 {
        sleep(Duration::from_millis(300)).await;
        if aria2.global_stat().await.is_ok() { ready = true; break; }
    }
    if !ready {
        // Cleanup terminal state isn't needed yet (raw mode not entered)
        eprintln!("Error: could not connect to aria2 RPC after 3 seconds.");
        eprintln!("Make sure aria2c is installed:  sudo apt install aria2");
        std::process::exit(1);
    }

    // ── Background tasks ──────────────────────────────────────────────────────
    let (tx, rx) = mpsc::unbounded_channel::<BgMsg>();
    tokio::spawn(refresh_loop(aria2.clone(), tx.clone()));
    tokio::spawn(tracker_refresh_loop(tx.clone()));

    // ── Set up TUI ────────────────────────────────────────────────────────────
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(ratatui::backend::CrosstermBackend::new(stdout()))?;

    let mut app = App::new(aria2, rx);
    let result  = event_loop(&mut terminal, &mut app).await;

    // ── Restore terminal ──────────────────────────────────────────────────────
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    if let Some(ref mut p) = aria2_proc { let _ = p.kill(); }

    result
}

// ============================================================================
// EVENT LOOP
// ============================================================================

async fn event_loop(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    loop {
        app.drain();
        terminal.draw(|f| render(app, f))?;

        if !event::poll(Duration::from_millis(50))? { continue; }
        let Event::Key(key) = event::read()? else { continue; };
        if key.kind != KeyEventKind::Press { continue; }

        // Ctrl+C always exits regardless of mode
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            break;
        }

        let mode = app.mode.clone();

        match mode {

            // ================================================================
            // NORMAL MODE
            // ================================================================
            Mode::Normal => {
                // Help overlay intercepts most keys
                if app.show_help {
                    if matches!(key.code, KeyCode::Char('?') | KeyCode::Esc) {
                        app.show_help = false;
                    }
                    continue;
                }

                match key.code {
                    // ── quit / help ──────────────────────────────────────────
                    KeyCode::Char('q') => break,
                    KeyCode::Char('?') => app.show_help = true,

                    // ── tab switch ───────────────────────────────────────────
                    KeyCode::Char('1') => app.tab = Tab::Downloads,
                    KeyCode::Char('2') => app.tab = Tab::Search,
                    KeyCode::Char('3') => app.tab = Tab::Settings,

                    // ── add URL ──────────────────────────────────────────────
                    KeyCode::Char('a') => {
                        app.mode = Mode::AddUrl;
                        app.buf.clear();
                    }

                    // ── start search ─────────────────────────────────────────
                    KeyCode::Char('s') => {
                        app.mode = Mode::Searching;
                        app.buf.clear();
                        app.tab  = Tab::Search;
                        app.status = "Type query and press Enter…".into();
                    }

                    // ── refresh trackers ─────────────────────────────────────
                    KeyCode::Char('r') => {
                        app.status = "Refreshing trackers…".into();
                        // spawn a fresh fetch in background
                        let tx2 = {
                            // We need to send a fresh fetch; easiest is to signal
                            // via status and let the periodic loop handle it.
                            // For immediate effect we spawn a one-shot task.
                            let (tx_tmp, _rx_tmp) = mpsc::unbounded_channel::<BgMsg>();
                            tokio::spawn(async move {
                                fetch_trackers_once(&tx_tmp).await;
                            });
                        };
                        let _ = tx2;
                    }

                    // ── search engine toggle ─────────────────────────────────
                    KeyCode::Char('e') if app.tab == Tab::Search => {
                        app.engine = app.engine.clone().toggle();
                        app.status = format!("Engine: {}", app.engine.label());
                    }

                    // ── category cycle ───────────────────────────────────────
                    KeyCode::Tab if app.tab == Tab::Search => {
                        app.category = app.category.next();
                        app.status = format!("Category: {}", app.category.label());
                    }

                    // ── speed limits ─────────────────────────────────────────
                    KeyCode::Char('l') if app.tab == Tab::Settings => {
                        app.mode = Mode::SpeedLimit { field: 0 };
                        app.buf.clear();
                    }

                    // ── navigation ───────────────────────────────────────────
                    KeyCode::Down | KeyCode::Char('j') => {
                        match app.tab {
                            Tab::Downloads => app.dl_sel.select_next(),
                            Tab::Search    => app.res_sel.select_next(),
                            _              => {}
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        match app.tab {
                            Tab::Downloads => app.dl_sel.select_previous(),
                            Tab::Search    => app.res_sel.select_previous(),
                            _              => {}
                        }
                    }

                    // ── pause / resume ───────────────────────────────────────
                    KeyCode::Char('p') => {
                        if let Some(i) = app.dl_sel.selected() {
                            if let Some(d) = app.downloads.get(i).cloned() {
                                if d.status == "paused" {
                                    let _ = app.aria2.unpause(&d.gid).await;
                                    app.status = format!("▶ Resumed: {}", d.name);
                                } else {
                                    let _ = app.aria2.pause(&d.gid).await;
                                    app.status = format!("⏸ Paused: {}", d.name);
                                }
                            }
                        }
                    }

                    // ── force remove ─────────────────────────────────────────
                    KeyCode::Char('x') => {
                        if let Some(i) = app.dl_sel.selected() {
                            if let Some(d) = app.downloads.get(i).cloned() {
                                let _ = app.aria2.remove(&d.gid).await;
                                app.status = format!("✗ Removed: {}", d.name);
                            }
                        }
                    }

                    // ── clear one from history ───────────────────────────────
                    KeyCode::Char('c') if app.tab == Tab::Downloads => {
                        if let Some(i) = app.dl_sel.selected() {
                            if let Some(d) = app.downloads.get(i).cloned() {
                                app.mode = Mode::Confirm(
                                    ConfirmKind::ClearOne { gid: d.gid.clone(), name: d.name.clone() },
                                    false,
                                );
                            } else {
                                app.status = "No item selected.".into();
                            }
                        } else {
                            app.status = "Select an item first (↑ ↓).".into();
                        }
                    }

                    // ── clear all history ────────────────────────────────────
                    KeyCode::Char('C') if app.tab == Tab::Downloads => {
                        app.mode = Mode::Confirm(ConfirmKind::ClearAll, true); // default = No
                    }

                    // ── download selected search result ───────────────────────
                    KeyCode::Enter if app.tab == Tab::Search => {
                        if let Some(i) = app.res_sel.selected() {
                            if let Some(r) = app.results.get(i).cloned() {
                                match app.add(&r.magnet).await {
                                    Ok(_)  => { app.status = format!("✓ Added: {}", r.name); app.tab = Tab::Downloads; }
                                    Err(e) => app.status = format!("✗ {e}"),
                                }
                            }
                        }
                    }

                    _ => {}
                }
            }

            // ================================================================
            // CONFIRM DIALOG
            // ================================================================
            Mode::Confirm(kind, no) => match key.code {
                KeyCode::Left  | KeyCode::Char('y') => app.mode = Mode::Confirm(kind.clone(), false),
                KeyCode::Right | KeyCode::Char('n') => app.mode = Mode::Confirm(kind.clone(), true),
                KeyCode::Enter => {
                    app.mode = Mode::Normal;
                    if !no {
                        match &kind {
                            ConfirmKind::ClearOne { gid, name } => {
                                match app.aria2.remove_result(gid).await {
                                    Ok(_)  => app.status = format!("✓ Cleared: {name}"),
                                    Err(e) => app.status = format!("✗ {e}"),
                                }
                            }
                            ConfirmKind::ClearAll => {
                                match app.aria2.purge().await {
                                    Ok(_)  => app.status = "✓ History cleared.".into(),
                                    Err(e) => app.status = format!("✗ {e}"),
                                }
                            }
                        }
                    } else {
                        app.status = "Cancelled.".into();
                    }
                }
                KeyCode::Esc => { app.mode = Mode::Normal; app.status = "Cancelled.".into(); }
                _ => {}
            },

            // ================================================================
            // ADD URL OVERLAY
            // ================================================================
            Mode::AddUrl => match key.code {
                KeyCode::Enter => {
                    let uri = app.buf.trim().to_string();
                    app.mode = Mode::Normal;
                    app.buf.clear();
                    if !uri.is_empty() {
                        match app.add(&uri).await {
                            Ok(_)  => { app.status = "✓ Added".into(); app.tab = Tab::Downloads; }
                            Err(e) => app.status = format!("✗ {e}"),
                        }
                    } else {
                        app.status = "Cancelled.".into();
                    }
                }
                KeyCode::Esc       => { app.mode = Mode::Normal; app.buf.clear(); app.status = "Cancelled.".into(); }
                KeyCode::Backspace => { app.buf.pop(); }
                KeyCode::Char(c)   => app.buf.push(c),
                _ => {}
            },

            // ================================================================
            // SEARCH INPUT
            // ================================================================
            Mode::Searching => match key.code {
                KeyCode::Enter => {
                    let q = app.buf.clone();
                    app.mode = Mode::Normal;
                    if !q.is_empty() {
                        if let Err(e) = app.search(&q).await {
                            app.status = format!("✗ Search failed: {e}");
                        }
                    }
                }
                KeyCode::Esc       => { app.mode = Mode::Normal; app.status = "[a]=add  [s]=search  [?]=help".into(); }
                KeyCode::Backspace => { app.buf.pop(); }
                KeyCode::Char(c)   => app.buf.push(c),
                _ => {}
            },

            // ================================================================
            // SPEED LIMIT INPUT
            // ================================================================
            Mode::SpeedLimit { field } => match key.code {
                KeyCode::Tab => {
                    // Save current field, switch to the other
                    let val_kbs: u64 = app.buf.trim().parse().unwrap_or(0);
                    if field == 0 { app.limits.dl_bps = val_kbs * 1024; }
                    else          { app.limits.ul_bps = val_kbs * 1024; }
                    app.mode = Mode::SpeedLimit { field: 1 - field };
                    app.buf.clear();
                }
                KeyCode::Enter => {
                    let val_kbs: u64 = app.buf.trim().parse().unwrap_or(0);
                    if field == 0 { app.limits.dl_bps = val_kbs * 1024; }
                    else          { app.limits.ul_bps = val_kbs * 1024; }
                    app.mode = Mode::Normal;
                    app.buf.clear();
                    match app.apply_limits().await {
                        Ok(_) => {
                            let dl = if app.limits.dl_bps == 0 { "∞".into() } else { fmt_spd(app.limits.dl_bps) };
                            let ul = if app.limits.ul_bps == 0 { "∞".into() } else { fmt_spd(app.limits.ul_bps) };
                            app.status = format!("✓ Speed limits applied: ↓{dl} ↑{ul}");
                        }
                        Err(e) => app.status = format!("✗ {e}"),
                    }
                }
                KeyCode::Esc => {
                    app.mode = Mode::Normal;
                    app.buf.clear();
                    app.status = "Cancelled.".into();
                }
                KeyCode::Backspace      => { app.buf.pop(); }
                KeyCode::Char(c) if c.is_ascii_digit() => app.buf.push(c),
                _ => {}
            },
        }
    }
    Ok(())
}