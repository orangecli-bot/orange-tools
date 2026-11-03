//! `air-client listen`: one long-lived resumable websocket (same air_server::Client / air::Air /
//! air::Instance path as Orange 1.0.1's ramp2 AirService + InstanceService). Server pushes new
//! channel slots; nothing polls. Every new message from someone else is appended to unread.jsonl
//! (kiwi_daemon.sh line shape). If a notify.env with AIR_WAKE_URL/AIR_WAKE_KEY is configured, a
//! batched wake POST follows. No auto-ack, no receipts, no re-wake retries.
use crate::common::*;
use crate::wake;
use air::{DefaultResolver, Id, Identity, Instance, Location, Name, Output, Request, RequestType, Resolver, Response, Secret};
use air_server::Client;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

macro_rules! log {
    ($($t:tt)*) => { eprintln!("{} {}", chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%z"), format!($($t)*)) };
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Target { Air(RequestType), Room(Id) }
struct Event { target: Target, stream: u64, response: Option<Response> }
struct Room { instance: Instance<ChatRoom>, live: bool, first_replay: bool }

#[derive(Serialize, Deserialize, Default)]
struct ListenState { baseline_ns: u64, rooms: HashMap<String, u64> }

struct Listener {
    secret: Secret, me: Name, leaks: Vec<String>, resolver: DefaultResolver,
    catalog: air::Air, catalog_ready: bool, buffered: Vec<(RequestType, Response)>,
    rooms: HashMap<Id, Room>, client: Option<Arc<Client>>,
    streams: HashMap<Target, (u64, JoinHandle<()>)>, next_stream: u64,
    events: mpsc::UnboundedSender<Event>,
    dir: PathBuf, cfg: Config, st: ListenState, import: Option<HashSet<String>>,
    contacts: HashMap<String, String>, wake: Option<Arc<wake::Wake>>,
    pending_wake: Vec<serde_json::Value>, wake_deadline: Option<tokio::time::Instant>,
}

impl Listener {
    async fn connect(&mut self) -> Arc<Client> {
        let server = Name::orange_me();
        let identity: Arc<Identity> = self.resolver.resolve(server, None).await;
        let mut delay = Duration::from_millis(250);
        loop {
            match Client::new((*identity).clone(), "wss").await {
                Ok(c) => { log!("connected: Air session open to {server} (wss, long-lived, resumable)"); return Arc::new(c); }
                Err(e) => log!("Air connection to {server} failed: {e}"),
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(10));
        }
    }
    async fn send(&mut self, target: Target, request: Request) {
        let client = self.client.clone().expect("client");
        let rx = client.send(request).await;
        let id = self.next_stream; self.next_stream += 1;
        let tx = self.events.clone();
        let handle = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(r) => if tx.send(Event { target, stream: id, response: Some(r) }).is_err() { return },
                    Err(_) => { let _ = tx.send(Event { target, stream: id, response: None }); return; }
                }
            }
        });
        if let Some((_, old)) = self.streams.insert(target, (id, handle)) { old.abort(); }
    }
    fn drop_stream(&mut self, t: Target) { if let Some((_, h)) = self.streams.remove(&t) { h.abort(); } }
    async fn start_all(&mut self) {
        for (_, (_, h)) in self.streams.drain() { h.abort(); }
        self.client = None;
        let c = self.connect().await;
        self.client = Some(c);
        for (k, r) in self.catalog.start() { self.send(Target::Air(k), r).await; }
        let ids: Vec<Id> = self.rooms.keys().copied().collect();
        for id in ids { let r = self.rooms.get_mut(&id).unwrap().instance.start(); self.send(Target::Room(id), r).await; }
        log!("subscribed: {} catalog channels, {} rooms", self.streams.len() - self.rooms.len(), self.rooms.len());
    }
    fn save_catalog(&self) {
        if let Err(e) = write_private(&self.dir.join("catalog.bin"), &postcard::to_allocvec(&self.catalog).unwrap()) { log!("could not persist catalog: {e}"); }
    }
    fn save_state(&self) {
        if let Err(e) = write_private(&self.dir.join("listen-state.json"), &serde_json::to_vec(&self.st).unwrap()) { log!("could not persist state: {e}"); }
    }
    async fn open_room(&mut self, location: Location) {
        if location.contract != <ChatRoom as air::Contract>::id() { return; }
        let id = Id::hash(&location);
        if self.rooms.contains_key(&id) { return; }
        private_dir(&self.dir.join("rooms")).ok();
        let saved = std::fs::read(self.room_file(&id)).ok().and_then(|b| postcard::from_bytes::<Instance<ChatRoom>>(&b).ok());
        let opened = match saved { Some(i) => Ok(i), None => Instance::<ChatRoom>::receive(self.secret.clone(), location) };
        match opened {
            Ok(mut instance) => {
                let request = instance.start();
                let first_replay = !self.st.rooms.contains_key(&id.to_string());
                self.rooms.insert(id, Room { instance, live: false, first_replay });
                log!("room {id}: following");
                if self.client.is_some() { self.send(Target::Room(id), request).await; }
            }
            Err(e) => log!("room {id}: cannot open: {e:?}"),
        }
    }
    fn room_file(&self, id: &Id) -> PathBuf { self.dir.join("rooms").join(format!("{id}.instance")) }
    fn save_room(&self, id: &Id) {
        if let Some(r) = self.rooms.get(id) {
            if let Err(e) = write_private(&self.room_file(id), &postcard::to_allocvec(&r.instance).unwrap()) { log!("could not persist room {id}: {e}"); }
        }
    }
    async fn catalog_response(&mut self, kind: RequestType, response: Response) {
        let subscribed = matches!(response, Response::Read(_, None));
        if let Some(location) = self.catalog.response(&mut self.resolver, kind, response).await {
            log!("catalog: new location for contract {}", location.contract);
            self.open_room(location).await;
        }
        for (k, r) in self.catalog.requests() { self.send(Target::Air(k), r).await; }
        if kind == RequestType::Storage && subscribed && !self.catalog_ready {
            self.catalog_ready = true;
            log!("catalog storage caught up; processing inbox");
            for (k, r) in std::mem::take(&mut self.buffered) { Box::pin(self.catalog_response(k, r)).await; }
        }
        self.save_catalog();
    }

    fn is_new(&self, room: &Room, rid: &str, t: &Text) -> bool {
        if let Some(after) = self.cfg.start_after_ns { if t.timestamp <= after { return false; } }
        if !room.first_replay { return t.timestamp > *self.st.rooms.get(rid).unwrap_or(&0); }
        match &self.import {
            Some(seen) => !seen.contains(&message_id(t.author, t.timestamp)),
            None => t.timestamp > self.st.baseline_ns,
        }
    }

    async fn room_response(&mut self, id: Id, response: Response) {
        let rid = id.to_string();
        let Some(room) = self.rooms.get_mut(&id) else { return };
        let output = room.instance.response(&mut self.resolver, response).await;
        let request = room.instance.request();
        match output {
            Output::Subscribed => {
                let room = self.rooms.get_mut(&id).unwrap();
                if !room.live { room.live = true; log!("room {id}: caught up, live"); }
                if room.first_replay { room.first_replay = false; self.st.rooms.entry(rid.clone()).or_insert(0); }
                self.save_state();
                self.save_room(&id);
            }
            Output::Read(Some(t), _) | Output::Created(Some(t), _) => {
                let room = self.rooms.get(&id).unwrap();
                let new = self.is_new(room, &rid, &t);
                let live = room.live;
                let wm = self.st.rooms.get(&rid).copied().unwrap_or(0);
                if t.timestamp > wm { self.st.rooms.insert(rid.clone(), t.timestamp); }
                if new && t.author != self.me { self.deliver(&rid, &t); }
                if live || new { self.save_state(); self.save_room(&id); }
            }
            _ => {}
        }
        if let Some(r) = request { self.send(Target::Room(id), r).await; }
    }

    fn deliver(&mut self, rid: &str, t: &Text) {
        let sender = t.author.to_string();
        let from = self.contacts.get(&sender).cloned().unwrap_or_else(|| "unknown".into());
        let leak = contains_leak(&t.text, &self.leaks);
        let content = if leak { "<redacted: contained private key material>".to_string() } else { t.text.clone() };
        let line = unread_line(&from, &sender, rid, t.timestamp, &content);
        let res = (|| -> R<()> {
            let _lock = Lock::take(&self.cfg.unread_file.with_extension("lock"))?;
            append_private(&self.cfg.unread_file, &format!("{}\n", serde_json::to_string(&line).unwrap()))
        })();
        let lag = now_ns().saturating_sub(t.timestamp) / 1_000_000;
        match res {
            Ok(()) => log!("message: room={rid} sender={sender} len={} server_write->received {lag} ms -> unread.jsonl", t.text.len()),
            Err(e) => log!("message: room={rid} sender={sender}: UNREAD APPEND FAILED: {e}"),
        }
        if leak || self.wake.is_none() { return; }
        self.pending_wake.push(match self.cfg.wake_format {
            WakeFormat::Kiwi => serde_json::json!({"sender": sender, "room": rid, "timestamp": t.timestamp}),
            WakeFormat::Orange => {
                let mut c: String = t.text.chars().take(2000).collect();
                if t.text.chars().count() > 2000 { c.push('…'); }
                serde_json::json!({"event": "orange_message", "source": "air-client", "me": self.me.to_string(),
                    "sender": sender, "room": rid, "content": c, "ts": t.timestamp / 1_000_000_000, "ts_ns": t.timestamp})
            }
        });
        if self.wake_deadline.is_none() {
            self.wake_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(self.cfg.wake_batch_ms));
        }
    }

    fn flush_wake(&mut self) {
        self.wake_deadline = None;
        let items = std::mem::take(&mut self.pending_wake);
        let Some(w) = self.wake.clone() else { return };
        if items.is_empty() { return; }
        let bodies: Vec<serde_json::Value> = match self.cfg.wake_format {
            WakeFormat::Kiwi => vec![serde_json::json!({"event": "air_message", "count": items.len(), "messages": items})],
            WakeFormat::Orange => items,
        };
        for body in bodies {
            let w = w.clone();
            tokio::spawn(async move {
                let bytes = serde_json::to_vec(&body).unwrap();
                let t = Instant::now();
                // Exactly one attempt (re-wake retries are not allowed); the message stays in unread.jsonl.
                match tokio::time::timeout(Duration::from_secs(10), wake::post(&w.url, &w.key, &bytes)).await {
                    Ok(Ok(s)) => log!("wake POST -> {s} in {} ms ({} message(s))", t.elapsed().as_millis(), body["count"].as_u64().unwrap_or(1)),
                    Ok(Err(e)) => log!("wake POST failed: {e} (not retried; message is in unread.jsonl)"),
                    Err(_) => log!("wake POST timed out (not retried; message is in unread.jsonl)"),
                }
            });
        }
    }

    async fn run(mut self, mut events: mpsc::UnboundedReceiver<Event>) {
        self.start_all().await;
        let mut restart_delay = Duration::from_secs(1);
        let mut session_started = Instant::now();
        let mut usr1 = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1()).expect("signal");
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
        loop {
            let deadline = self.wake_deadline;
            let event = tokio::select! {
                e = events.recv() => e,
                _ = async { tokio::time::sleep_until(deadline.unwrap()).await }, if deadline.is_some() => { self.flush_wake(); continue; }
                _ = usr1.recv() => { log!("SIGUSR1: simulating a closed Air session"); self.start_all().await; session_started = Instant::now(); continue; }
                _ = term.recv() => { log!("SIGTERM, exiting"); self.flush_wake(); tokio::time::sleep(Duration::from_millis(500)).await; return; }
                _ = tokio::signal::ctrl_c() => { log!("SIGINT, exiting"); return; }
            };
            let Some(Event { target, stream, response }) = event else { return };
            if self.streams.get(&target).map(|s| s.0) != Some(stream) { continue; }
            match (target, response) {
                (_, None) => {
                    log!("Air session closed a response stream ({target:?}); re-establishing session");
                    if session_started.elapsed() > Duration::from_secs(60) { restart_delay = Duration::from_secs(1); }
                    tokio::time::sleep(restart_delay).await;
                    restart_delay = (restart_delay * 2).min(Duration::from_secs(10));
                    self.start_all().await;
                    session_started = Instant::now();
                }
                (Target::Air(kind), Some(r)) => {
                    if !matches!(r, Response::Read(_, None)) { self.drop_stream(target); }
                    if matches!(kind, RequestType::Inbox(_)) && !self.catalog_ready { self.buffered.push((kind, r)); }
                    else { self.catalog_response(kind, r).await; }
                }
                (Target::Room(id), Some(r)) => self.room_response(id, r).await,
            }
        }
    }
}

pub async fn listen(cfg: Config) -> R<()> {
    let (secret, leaks) = load_identity(&cfg.identity_file)?;
    let me = secret.name();
    private_dir(&cfg.state_dir)?;
    let dir = cfg.state_dir.join("listen");
    private_dir(&dir)?;
    let _single = Lock::try_take(&dir.join("listener.lock"))?.ok_or("another air-client listen is already running for this state dir")?;
    let wake = wake::load(cfg.notify_env.as_deref())?.map(Arc::new);
    let catalog_path = dir.join("catalog.bin");
    let catalog = match std::fs::read(&catalog_path).ok().and_then(|b| postcard::from_bytes::<air::Air>(&b).ok()) {
        Some(c) => { log!("catalog restored from state"); c }
        None => { log!("catalog: starting fresh from identity"); air::Air::new(secret.clone()).map_err(|e| format!("{e:?}"))? }
    };
    let st = match std::fs::read(dir.join("listen-state.json")).ok().and_then(|b| serde_json::from_slice::<ListenState>(&b).ok()) {
        Some(s) => s,
        None => ListenState { baseline_ns: cfg.start_after_ns.unwrap_or_else(now_ns), rooms: HashMap::new() },
    };
    let import = match &cfg.import_seen {
        Some(p) => {
            let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?)
                .map_err(|e| format!("{}: {e}", p.display()))?;
            let ids: HashSet<String> = v["ids"].as_array().ok_or("AIR_IMPORT_SEEN: no ids array")?
                .iter().filter_map(|x| x.as_str().map(String::from)).collect();
            log!("imported {} seen message ids (kiwi_seen.json) for first replay of each room", ids.len());
            Some(ids)
        }
        None => None,
    };
    let contacts = load_contacts(cfg.contacts_file.as_deref());
    let (tx, rx) = mpsc::unbounded_channel();
    let mut l = Listener {
        secret, me, leaks, resolver: DefaultResolver::default(), catalog, catalog_ready: false, buffered: vec![],
        rooms: HashMap::new(), client: None, streams: HashMap::new(), next_stream: 0, events: tx,
        dir, st, import, contacts, wake, pending_wake: vec![], wake_deadline: None, cfg,
    };
    l.save_state();
    for loc in l.catalog.list(&<ChatRoom as air::Contract>::id()) { l.open_room(loc).await; }
    log!("air-client listen starting for {me}; {} known rooms; wake {}; unread -> {}", l.rooms.len(),
        match (&l.wake, l.cfg.wake_format) { (None, _) => "off".to_string(), (Some(_), f) => format!("on ({f:?} format, batch {} ms)", l.cfg.wake_batch_ms) },
        l.cfg.unread_file.display());
    l.run(rx).await;
    Ok(())
}
