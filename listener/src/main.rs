//! orange-listener: a headless, event-driven Air listener for one Orange identity.
//!
//! It speaks to the Air server through the *same* code path as Orange 1.0.1:
//!   ramp2 3.0.4 `Network::client` -> `air_server::Client::new(identity, "wss")` (one resumable,
//!   long-lived websocket to air.orange.me:443), and the same `air::Air` catalog (inbox +
//!   storage channels) and `air::Instance<ChatRoom>` room channels that ramp2's AirService /
//!   InstanceService drive. A channel `Read` on the next, not-yet-written slot is a server-side
//!   subscription: the server answers `Read(_, None)` and later *pushes* `Read(_, Some(..))` the
//!   moment someone writes it. Nothing here polls.
//!
//! Recovery mirrors Orange's design: transport drops are handled inside air_server::Client
//! (1 s retry + session resume token). If the client session is ever *rejected* (the case where
//! Orange panics at ramp2 air.rs:97 "Air closed a response stream"), we do what an Orange restart
//! does: open a fresh Client with ramp2's connect backoff (250 ms doubling to 10 s) and re-issue
//! `start()` for every channel from the persisted cursor - without crashing.
//!
//! Secrets: the identity is read from a mode-600 file; the webhook key from a mode-600 env file.
//! Neither is ever logged.

use air::{
    Contract, DefaultResolver, Id, Identity, Instance, Location, Metadata, Name, Output, Request,
    RequestType, Resolver, Response, Secret,
};
use air_server::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

macro_rules! log {
    ($($t:tt)*) => {
        eprintln!("{} {}", chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%z"), format!($($t)*))
    };
}

// ---------------------------------------------------------------------------------------------
// Wire-compatible mirror of chk's ChatRoom contract (chk rev 2da2ba1, src/air/messages.rs).
// Same contract id, same Init type (Id), and the first two ChatRoomAction variants with the same
// positions/types, so postcard decoding and the derived Hash (used for signature checks) match.
// Other actions (stickers, photos, group ops...) fail to decode -> Output::Garbage; the channel
// cursor still advances, so they are skipped without stalling.
// ---------------------------------------------------------------------------------------------
#[derive(Debug, Clone, Hash, Serialize, Deserialize)]
pub enum ChatRoomAction {
    Share(Name),
    SendMessage(String),
}

#[derive(Debug, Clone, Hash, Serialize, Deserialize)]
pub struct ChatRoom {
    pub members: Vec<Name>,
}

#[derive(Debug, Clone)]
pub struct Text {
    pub author: Name,
    pub timestamp: u64,
    pub text: String,
}

impl Contract for ChatRoom {
    type Init = Id;
    type Message = ChatRoomAction;
    type Result = Option<Text>;
    fn id() -> Id { Id::hash("ChatRoom") }
    fn init(_init: Id, metadata: Metadata) -> Self { ChatRoom { members: vec![metadata.signer] } }
    fn apply(&mut self, message: ChatRoomAction, metadata: Metadata) -> Option<Text> {
        match message {
            ChatRoomAction::Share(name) => {
                if !self.members.contains(&name) { self.members.push(name); }
                None
            }
            ChatRoomAction::SendMessage(text) => Some(Text {
                author: metadata.signer, timestamp: metadata.timestamp, text,
            }),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------------------------
struct Config {
    identity_file: PathBuf,
    state_dir: PathBuf,
    seed_state: Option<PathBuf>,
    wake_url: String,
    wake_key: String,
}

fn check_private(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!("{} must not be group/other accessible (chmod 600)", path.display()));
    }
    Ok(())
}

fn parse_env_file(path: &Path) -> Result<HashMap<String, String>, String> {
    check_private(path)?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim();
            let v = v.strip_prefix('"').and_then(|v| v.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(v);
            map.insert(k.trim().to_string(), v.to_string());
        }
    }
    Ok(map)
}

fn load_config() -> Result<Config, String> {
    let args: Vec<String> = std::env::args().collect();
    let path = args.iter().position(|a| a == "--config").and_then(|i| args.get(i + 1)).cloned()
        .or_else(|| std::env::var("ORANGE_LISTENER_CONFIG").ok());
    // Config file (mode 600) if given; otherwise / additionally the process environment.
    let mut env = match &path { Some(p) => parse_env_file(Path::new(p))?, None => HashMap::new() };
    for k in ["ORANGE_IDENTITY_FILE", "STATE_DIR", "SEED_STATE_FILE", "WAKE_ENV_FILE", "AIR_WAKE_URL", "AIR_WAKE_KEY"] {
        if let Ok(v) = std::env::var(k) { env.entry(k.to_string()).or_insert(v); }
    }
    if path.is_none() && !env.contains_key("ORANGE_IDENTITY_FILE") {
        return Err("usage: orange-listener --config /path/to/orange-listener.env (mode 600), or set ORANGE_IDENTITY_FILE, STATE_DIR, AIR_WAKE_URL and AIR_WAKE_KEY / WAKE_ENV_FILE in the environment".into());
    }
    // Optional separate mode-600 file holding AIR_WAKE_URL / AIR_WAKE_KEY.
    if let Some(wake) = env.get("WAKE_ENV_FILE").cloned() {
        for (k, v) in parse_env_file(Path::new(&wake))? { env.entry(k).or_insert(v); }
    }
    let get = |k: &str| env.get(k).filter(|v| !v.is_empty()).cloned();
    Ok(Config {
        identity_file: get("ORANGE_IDENTITY_FILE").ok_or("ORANGE_IDENTITY_FILE missing")?.into(),
        state_dir: get("STATE_DIR").ok_or("STATE_DIR missing")?.into(),
        seed_state: get("SEED_STATE_FILE").map(PathBuf::from),
        wake_url: get("AIR_WAKE_URL").ok_or("AIR_WAKE_URL missing")?,
        wake_key: get("AIR_WAKE_KEY").ok_or("AIR_WAKE_KEY missing")?,
    })
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true)
            .mode(0o600).open(&tmp)?;
        std::io::Write::write_all(&mut f, bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

// ---------------------------------------------------------------------------------------------
// Webhook (in-process HTTPS POST; the key never appears in argv or logs)
// ---------------------------------------------------------------------------------------------
async fn post_once(url: &str, key: &str, body: &[u8]) -> Result<u16, String> {
    let rest = url.strip_prefix("https://").ok_or("AIR_WAKE_URL must be https://")?;
    let (hostport, path) = match rest.find('/') { Some(i) => (&rest[..i], &rest[i..]), None => (rest, "/") };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|e| e.to_string())?),
        None => (hostport, 443),
    };
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions().map_err(|e| e.to_string())?
        .with_root_certificates(roots).with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect((host, port)).await.map_err(|e| e.to_string())?;
    tcp.set_nodelay(true).ok();
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| e.to_string())?;
    let mut tls = connector.connect(name, tcp).await.map_err(|e| e.to_string())?;
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nUser-Agent: orange-listener/0.1\r\nConnection: close\r\n\r\n",
        body.len()
    );
    tls.write_all(head.as_bytes()).await.map_err(|e| e.to_string())?;
    tls.write_all(body).await.map_err(|e| e.to_string())?;
    tls.flush().await.map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 512];
    let mut n = 0;
    while n < 12 {
        let r = tls.read(&mut buf[n..]).await.map_err(|e| e.to_string())?;
        if r == 0 { break; }
        n += r;
    }
    let status = String::from_utf8_lossy(&buf[..n]);
    status.split_whitespace().nth(1).and_then(|s| s.parse().ok())
        .ok_or_else(|| "no HTTP status".to_string())
}

fn fire_webhook(url: String, key: Arc<String>, payload: serde_json::Value) {
    tokio::spawn(async move {
        let body = serde_json::to_vec(&payload).unwrap();
        let mut delay = Duration::from_secs(1);
        for attempt in 1..=6 {
            let started = Instant::now();
            match tokio::time::timeout(Duration::from_secs(20), post_once(&url, &key, &body)).await {
                Ok(Ok(status)) if (200..300).contains(&status) => {
                    log!("webhook POST ok status={status} in {} ms (room={} sender={})",
                        started.elapsed().as_millis(), payload["room"], payload["sender"]);
                    return;
                }
                Ok(Ok(status)) => log!("webhook POST attempt {attempt} got HTTP {status}"),
                Ok(Err(e)) => log!("webhook POST attempt {attempt} failed: {e}"),
                Err(_) => log!("webhook POST attempt {attempt} timed out"),
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(30));
        }
        log!("webhook POST gave up after retries");
    });
}

// ---------------------------------------------------------------------------------------------
// Listener state
// ---------------------------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Target { Air(RequestType), Room(Id) }

struct Event { target: Target, stream: u64, response: Option<Response> }

struct Room { instance: Instance<ChatRoom>, live: bool }

#[derive(Serialize, Deserialize, Default)]
struct Watermark { last_ts: u64 }

struct Listener {
    secret: Secret,
    me: Name,
    resolver: DefaultResolver,
    catalog: air::Air,
    catalog_ready: bool,
    buffered: Vec<(RequestType, Response)>,
    rooms: HashMap<Id, Room>,
    client: Option<Arc<Client>>,
    streams: HashMap<Target, (u64, JoinHandle<()>)>,
    next_stream: u64,
    events: mpsc::UnboundedSender<Event>,
    state_dir: PathBuf,
    watermark: Watermark,
    wake_url: String,
    wake_key: Arc<String>,
}

impl Listener {
    /// ramp2 3.0.4 air.rs:521-535 `Network::client`: resolve the server, then
    /// `Client::new(identity, "wss")` with 250 ms doubling backoff capped at 10 s.
    async fn connect(&mut self) -> Arc<Client> {
        let server = Name::orange_me();
        let identity: Arc<Identity> = self.resolver.resolve(server, None).await;
        let mut delay = Duration::from_millis(250);
        loop {
            match Client::new((*identity).clone(), "wss").await {
                Ok(client) => {
                    log!("connected: Air session open to {server} (wss, long-lived, resumable)");
                    return Arc::new(client);
                }
                Err(error) => log!("Air connection to {server} failed: {error}"),
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(10));
        }
    }

    /// Equivalent of ramp2 `Air::send` + replacing `Responses`: dropping the previous receiver
    /// cancels that subscription in air_server::Client, exactly as ramp2 does by overwriting it.
    async fn send(&mut self, target: Target, request: Request) {
        let client = self.client.clone().expect("client");
        let rx = client.send(request).await;
        let id = self.next_stream;
        self.next_stream += 1;
        let tx = self.events.clone();
        let handle = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(response) => {
                        if tx.send(Event { target, stream: id, response: Some(response) }).is_err() { return; }
                    }
                    Err(_) => { let _ = tx.send(Event { target, stream: id, response: None }); return; }
                }
            }
        });
        if let Some((_, old)) = self.streams.insert(target, (id, handle)) { old.abort(); }
    }

    fn drop_stream(&mut self, target: Target) {
        if let Some((_, h)) = self.streams.remove(&target) { h.abort(); }
    }

    /// (Re)start every channel from its current cursor: what ramp2 does on app start
    /// (AirService::init -> Air::start, InstanceService::init -> Instance::start).
    async fn start_all(&mut self) {
        for (_, (_, h)) in self.streams.drain() { h.abort(); }
        self.client = None;
        let client = self.connect().await;
        self.client = Some(client);
        let requests = self.catalog.start();
        for (kind, request) in requests { self.send(Target::Air(kind), request).await; }
        let ids: Vec<Id> = self.rooms.keys().copied().collect();
        for id in ids {
            let request = self.rooms.get_mut(&id).unwrap().instance.start();
            self.send(Target::Room(id), request).await;
        }
        log!("subscribed: {} catalog channels, {} rooms", self.streams.len() - self.rooms.len(), self.rooms.len());
    }

    fn save_catalog(&self) {
        match postcard::to_allocvec(&self.catalog) {
            Ok(bytes) => if let Err(e) = write_private(&self.state_dir.join("catalog.bin"), &bytes) {
                log!("could not persist catalog: {e}");
            },
            Err(e) => log!("could not serialize catalog: {e}"),
        }
    }

    fn save_watermark(&self) {
        let _ = write_private(&self.state_dir.join("watermark.json"),
            &serde_json::to_vec(&self.watermark).unwrap());
    }

    async fn open_room(&mut self, location: Location) {
        if location.contract != ChatRoom::id() { return; }
        let id = Id::hash(&location);
        if self.rooms.contains_key(&id) { return; }
        match Instance::<ChatRoom>::receive(self.secret.clone(), location) {
            Ok(mut instance) => {
                let request = instance.start();
                self.rooms.insert(id, Room { instance, live: false });
                log!("room {id}: following");
                if self.client.is_some() { self.send(Target::Room(id), request).await; }
            }
            Err(e) => log!("room {id}: cannot open: {e:?}"),
        }
    }

    /// ramp2 AirService::handle (air.rs:506-512).
    async fn catalog_response(&mut self, kind: RequestType, response: Response) {
        let subscribed = matches!(response, Response::Read(_, None));
        if let Some(location) = self.catalog.response(&mut self.resolver, kind, response).await {
            log!("catalog: new location for contract {}", location.contract);
            self.open_room(location).await;
        }
        let requests = self.catalog.requests();
        for (kind, request) in requests { self.send(Target::Air(kind), request).await; }
        if kind == RequestType::Storage && subscribed && !self.catalog_ready {
            self.catalog_ready = true;
            log!("catalog storage caught up; processing inbox");
            for (kind, response) in std::mem::take(&mut self.buffered) {
                Box::pin(self.catalog_response(kind, response)).await;
            }
        }
        self.save_catalog();
    }

    /// ramp2 InstanceService::handle (air.rs:424-440), plus the webhook.
    async fn room_response(&mut self, id: Id, response: Response) {
        let Some(room) = self.rooms.get_mut(&id) else { return };
        let output = room.instance.response(&mut self.resolver, response).await;
        let mut wake = None;
        let mut advanced = false;
        match output {
            Output::Subscribed => {
                if !room.live { room.live = true; log!("room {id}: caught up, live"); }
            }
            Output::Read(Some(text), _) | Output::Created(Some(text), _) => {
                // Only messages newer than the persisted watermark wake the agent: history replayed
                // at startup is skipped, messages that arrived while we were down are delivered.
                if text.timestamp > self.watermark.last_ts {
                    if text.author != self.me { wake = Some(text.clone()); }
                    self.watermark.last_ts = text.timestamp;
                    advanced = true;
                }
            }
            _ => {}
        }
        let request = room.instance.request();
        if advanced { self.save_watermark(); }
        if let Some(text) = wake {
            let mut content: String = text.text.chars().take(2000).collect();
            if text.text.chars().count() > 2000 { content.push_str("…"); }
            let now = chrono::Utc::now().timestamp_nanos_opt().unwrap() as u64;
            log!("message: room={id} sender={} len={} server_write->received {} ms -> firing webhook",
                text.author, text.text.len(), now.saturating_sub(text.timestamp) / 1_000_000);
            fire_webhook(self.wake_url.clone(), self.wake_key.clone(), serde_json::json!({
                "event": "orange_message",
                "source": "orange-listener",
                "me": self.me.to_string(),
                "sender": text.author.to_string(),
                "room": id.to_string(),
                "content": content,
                "ts": text.timestamp / 1_000_000_000,
                "ts_ns": text.timestamp,
            }));
        }
        if let Some(request) = request { self.send(Target::Room(id), request).await; }
    }

    async fn run(mut self, mut events: mpsc::UnboundedReceiver<Event>) {
        self.start_all().await;
        let mut restart_delay = Duration::from_secs(1);
        let mut session_started = Instant::now();
        // Test hook: SIGUSR1 forces the same path as a rejected/closed Air session.
        let mut usr1 = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
            .expect("signal handler");
        loop {
            let event = tokio::select! {
                event = events.recv() => event,
                _ = usr1.recv() => {
                    log!("SIGUSR1: simulating a closed Air session");
                    self.start_all().await;
                    session_started = Instant::now();
                    continue;
                }
            };
            let Some(Event { target, stream, response }) = event else { return };
            if self.streams.get(&target).map(|s| s.0) != Some(stream) { continue; } // stale stream
            match (target, response) {
                (_, None) => {
                    // air_server::Client ended the session (rejected) - Orange would panic at
                    // ramp2 air.rs:97 here. Instead: fresh Client + start() all, like a restart.
                    log!("Air session closed a response stream ({target:?}); re-establishing session");
                    if session_started.elapsed() > Duration::from_secs(60) { restart_delay = Duration::from_secs(1); }
                    tokio::time::sleep(restart_delay).await;
                    restart_delay = (restart_delay * 2).min(Duration::from_secs(10));
                    self.start_all().await;
                    session_started = Instant::now();
                }
                (Target::Air(kind), Some(response)) => {
                    if !matches!(response, Response::Read(_, None)) { self.drop_stream(target); }
                    if matches!(kind, RequestType::Inbox(_)) && !self.catalog_ready {
                        self.buffered.push((kind, response));
                    } else {
                        self.catalog_response(kind, response).await;
                    }
                }
                (Target::Room(id), Some(response)) => self.room_response(id, response).await,
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = match load_config() {
        Ok(c) => c,
        Err(e) => { log!("config error: {e}"); std::process::exit(2); }
    };
    if let Err(e) = check_private(&config.identity_file) { log!("{e}"); std::process::exit(2); }
    let secret: Secret = match std::fs::read_to_string(&config.identity_file).ok()
        .and_then(|s| serde_json::from_str(s.trim()).ok()) {
        Some(s) => s,
        None => { log!("cannot parse identity file {}", config.identity_file.display()); std::process::exit(2); }
    };
    // Same validity check Orange applies when loading "ramp.air.identity" (ramp2 lib.rs:35-38).
    if !secret.path().is_empty() || secret.name().to_string() != secret.public(None).public_key().to_string() {
        log!("identity file does not contain a root Orange identity"); std::process::exit(2);
    }
    let me = secret.name();
    std::fs::create_dir_all(&config.state_dir).ok();
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&config.state_dir, std::fs::Permissions::from_mode(0o700));
    }
    // Catalog (inbox + storage cursors): our own persisted copy, else a seed snapshot of the
    // Orange app's AirState, else fresh from the identity.
    let catalog_path = config.state_dir.join("catalog.bin");
    let mut catalog: Option<air::Air> = None;
    for (label, path) in [("state", Some(catalog_path.clone())), ("seed", config.seed_state.clone())] {
        let Some(path) = path else { continue };
        if catalog.is_some() || !path.exists() { continue; }
        if check_private(&path).is_err() { log!("{} is not mode 600; ignoring", path.display()); continue; }
        match std::fs::read(&path).ok().and_then(|b| postcard::from_bytes::<air::Air>(&b).ok()) {
            Some(c) => { log!("catalog restored from {label} file"); catalog = Some(c); }
            None => log!("could not decode {label} file {}; ignoring", path.display()),
        }
    }
    let catalog = catalog.unwrap_or_else(|| {
        log!("catalog: starting fresh from identity");
        air::Air::new(secret.clone()).expect("Could not create the Air state")
    });
    let watermark_path = config.state_dir.join("watermark.json");
    let watermark = std::fs::read(&watermark_path).ok()
        .and_then(|b| serde_json::from_slice::<Watermark>(&b).ok())
        .unwrap_or_else(|| {
            // First run: only messages from now on wake the agent.
            Watermark { last_ts: chrono::Utc::now().timestamp_nanos_opt().unwrap() as u64 }
        });
    let (tx, rx) = mpsc::unbounded_channel();
    let mut listener = Listener {
        secret, me, resolver: DefaultResolver::default(), catalog, catalog_ready: false,
        buffered: vec![], rooms: HashMap::new(), client: None, streams: HashMap::new(),
        next_stream: 0, events: tx, state_dir: config.state_dir.clone(), watermark,
        wake_url: config.wake_url, wake_key: Arc::new(config.wake_key),
    };
    listener.save_watermark();
    let known = listener.catalog.list(&ChatRoom::id());
    for location in known { listener.open_room(location).await; }
    log!("orange-listener starting for {me}; {} known rooms", listener.rooms.len());
    tokio::select! {
        _ = listener.run(rx) => {}
        _ = tokio::signal::ctrl_c() => log!("SIGINT, exiting"),
    }
}
