//! orange-send: headless send/read for one Orange identity, without the GUI.
//!
//! Same code path and wire format as Orange 1.0.1 (ramp2 3.0.4 + chk rev 2da2ba1):
//!   * transport: air_server 2.0.1 `Client::new(<air.orange.me identity>, "wss")` (anonymous,
//!     resumable websocket; requests are authorised by channel keys, not by a login session);
//!   * room: air 4.0.1 `Instance<ChatRoom>` opened with `Instance::receive(secret, location)`;
//!   * message: `ChatRoomAction::SendMessage(String)` (variant #1, exactly what chk's
//!     MessagesPage sends from the message bar), wrapped by air in `Signed<Message<C>>`, signed
//!     with the identity's room-derived key, encrypted for the channel slot and written with
//!     `Request::Create` to the next free slot. A slot taken concurrently by another client (e.g.
//!     the running Orange GUI) makes the server answer with that slot's content; air re-queues our
//!     message and writes it to the following slot (first writer wins, nothing is lost).
//!
//! Secrets: the identity file and all per-room state (which embeds derived keys) live in mode-600
//! files under a mode-700 directory. Nothing secret is ever printed.

use air::{Contract, DefaultResolver, Id, Identity, Instance, Location, Metadata, Name, Output,
    Request, RequestType, Resolver, Response, Secret};
use air_server::Client;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ---- chk ChatRoom contract, wire-compatible subset (same as orange-listener) -------------------
// Contract id "ChatRoom", Init = Id, variants 0 and 1 at the same positions/types as chk's
// ChatRoomAction, so postcard bytes and the derived Hash (signed) are identical for SendMessage.
// Stickers/GIFs/photos/group ops do not decode -> Output::Garbage, cursor still advances.
#[derive(Debug, Clone, Hash, Serialize, Deserialize)]
pub enum ChatRoomAction { Share(Name), SendMessage(String) }

#[derive(Debug, Clone, Hash, Serialize, Deserialize)]
pub struct ChatRoom { pub members: Vec<Name> }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Text { pub author: Name, pub timestamp: u64, pub text: String }

impl Contract for ChatRoom {
    type Init = Id;
    type Message = ChatRoomAction;
    type Result = Option<Text>;
    fn id() -> Id { Id::hash("ChatRoom") }
    fn init(_: Id, m: Metadata) -> Self { ChatRoom { members: vec![m.signer] } }
    fn apply(&mut self, message: ChatRoomAction, m: Metadata) -> Option<Text> {
        match message {
            ChatRoomAction::Share(n) => { if !self.members.contains(&n) { self.members.push(n); } None }
            ChatRoomAction::SendMessage(text) => Some(Text { author: m.signer, timestamp: m.timestamp, text }),
        }
    }
}

// ---- helpers -----------------------------------------------------------------------------------
type R<T> = Result<T, String>;
const TIMEOUT: Duration = Duration::from_secs(20);

fn private_dir(p: &Path) -> R<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())
}
fn check_private(p: &Path) -> R<()> {
    use std::os::unix::fs::PermissionsExt;
    let m = std::fs::metadata(p).map_err(|e| format!("{}: {e}", p.display()))?;
    if m.permissions().mode() & 0o077 != 0 { return Err(format!("{} must be mode 600", p.display())); }
    Ok(())
}
fn write_private(p: &Path, bytes: &[u8]) -> R<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = p.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600)
        .open(&tmp).map_err(|e| e.to_string())?;
    f.write_all(bytes).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, p).map_err(|e| e.to_string())
}
fn ms(d: Duration) -> f64 { (d.as_secs_f64() * 1000.0 * 10.0).round() / 10.0 }
fn now_ns() -> u64 { chrono::Utc::now().timestamp_nanos_opt().unwrap() as u64 }

struct Lock(std::fs::File);
impl Lock {
    fn take(p: &Path) -> R<Self> {
        use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
        let f = std::fs::OpenOptions::new().write(true).create(true).mode(0o600).open(p)
            .map_err(|e| e.to_string())?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 { return Err("flock failed".into()); }
        Ok(Lock(f))
    }
}

struct Ctx { secret: Secret, me: Name, state: PathBuf, resolver: DefaultResolver }

impl Ctx {
    async fn client(&mut self) -> R<Client> {
        let server: std::sync::Arc<Identity> = self.resolver.resolve(Name::orange_me(), None).await;
        tokio::time::timeout(TIMEOUT, Client::new((*server).clone(), "wss")).await
            .map_err(|_| "connect timeout".to_string())?
    }
    fn locations_path(&self) -> PathBuf { self.state.join("locations.bin") }
    fn load_locations(&self) -> Vec<Location> {
        std::fs::read(self.locations_path()).ok()
            .and_then(|b| postcard::from_bytes(&b).ok()).unwrap_or_default()
    }

    /// Read-only catalog refresh: replays the identity's STORAGE channel (the list of instance
    /// locations Orange has registered) from a fresh air::Air. Inbox channels are never touched
    /// and a fresh Air has an empty storage queue, so this can only issue Request::Read.
    async fn refresh_rooms(&mut self, client: &Client) -> R<(Vec<Location>, usize)> {
        let mut catalog = air::Air::new(self.secret.clone()).map_err(|e| format!("{e:?}"))?;
        let mut request = catalog.start().remove(&RequestType::Storage).ok_or("no storage request")?;
        let mut reads = 0;
        loop {
            if !matches!(request, Request::Read(_)) { return Err("refusing: catalog wanted to write".into()); }
            let rx = client.send(request).await;
            let resp = tokio::time::timeout(TIMEOUT, rx.recv()).await.map_err(|_| "storage read timeout")?
                .map_err(|_| "storage stream closed")?;
            reads += 1;
            let done = matches!(resp, Response::Read(_, None));
            catalog.response(&mut self.resolver, RequestType::Storage, resp).await;
            if done { break; }
            request = match catalog.requests().remove(&RequestType::Storage) { Some(r) => r, None => break };
        }
        let rooms: Vec<Location> = catalog.list(&ChatRoom::id()).into_iter().collect();
        write_private(&self.locations_path(), &postcard::to_allocvec(&rooms).unwrap())?;
        Ok((rooms, reads))
    }

    fn find_room(&self, room: &str) -> Option<Location> {
        self.load_locations().into_iter().find(|l| Id::hash(l).to_string() == room)
    }

    fn room_paths(&self, id: &str) -> (PathBuf, PathBuf, PathBuf) {
        let d = self.state.join("rooms");
        (d.join(format!("{id}.instance")), d.join(format!("{id}.jsonl")), d.join(format!("{id}.lock")))
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct HistLine { ts_ns: u64, author: String, mine: bool, text: String, id: String }

struct RoomRun { inst: Instance<ChatRoom>, new: Vec<HistLine>, reads: usize, garbage: usize }

impl RoomRun {
    fn record(&mut self, me: Name, t: &Text) {
        // Same message id chk uses: Id::hash(&(signer, timestamp)).
        let id = Id::hash(&(t.author, t.timestamp)).to_string();
        self.new.push(HistLine { ts_ns: t.timestamp, author: t.author.to_string(), mine: t.author == me,
            text: t.text.clone(), id });
    }

    /// Drive the channel: catch up to the head (subscription answer) and, if `outgoing` is set,
    /// write it and wait for the server's Created. Returns the server timestamp of our write.
    async fn drive(&mut self, ctx: &mut Ctx, client: &Client, outgoing: Option<String>) -> R<Option<u64>> {
        let mut outgoing = outgoing;
        let mut sending = false;
        let mut request = self.inst.start();
        loop {
            let rx = client.send(request).await;
            let resp = tokio::time::timeout(TIMEOUT, rx.recv()).await.map_err(|_| "room request timeout")?
                .map_err(|_| "room stream closed")?;
            self.reads += 1;
            match self.inst.response(&mut ctx.resolver, resp).await {
                Output::Subscribed => {
                    if let Some(text) = outgoing.take() {
                        let room = self.inst.confirmed().ok_or("room has no Init (wrong location?)")?;
                        if !room.members.contains(&ctx.me) {
                            eprintln!("warning: identity not in the decoded member list (group rooms track members via variants this CLI does not decode)");
                        }
                        self.inst.send(ChatRoomAction::SendMessage(text));
                        sending = true;
                    } else if !sending { return Ok(None); }
                }
                Output::Created(r, _) => {
                    if let Some(t) = r { self.record(ctx.me, &t); }
                    if sending { return Ok(self.new.last().map(|l| l.ts_ns)); }
                }
                Output::Read(r, _) => { if let Some(t) = r { self.record(ctx.me, &t); } }
                Output::Init(_) => {}
                Output::Garbage => self.garbage += 1,
            }
            request = match self.inst.request() { Some(r) => r, None => return Err("channel idle unexpectedly".into()) };
        }
    }
}

async fn open_room(ctx: &mut Ctx, room: &str) -> R<(RoomRun, Lock, PathBuf, PathBuf)> {
    private_dir(&ctx.state.join("rooms"))?;
    let (ipath, hpath, lpath) = ctx.room_paths(room);
    let lock = Lock::take(&lpath)?;
    let inst = match std::fs::read(&ipath).ok().and_then(|b| postcard::from_bytes::<Instance<ChatRoom>>(&b).ok()) {
        Some(i) => i,
        None => {
            let loc = ctx.find_room(room).ok_or("unknown room id (run `orange-send rooms` first)")?;
            Instance::<ChatRoom>::receive(ctx.secret.clone(), loc).map_err(|e| format!("{e:?}"))?
        }
    };
    Ok((RoomRun { inst, new: vec![], reads: 0, garbage: 0 }, lock, ipath, hpath))
}

fn persist(run: &RoomRun, ipath: &Path, hpath: &Path) -> R<()> {
    write_private(ipath, &postcard::to_allocvec(&run.inst).unwrap())?;
    if !run.new.is_empty() {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(hpath)
            .map_err(|e| e.to_string())?;
        for l in &run.new { writeln!(f, "{}", serde_json::to_string(l).unwrap()).map_err(|e| e.to_string())?; }
    }
    Ok(())
}

fn history(hpath: &Path) -> Vec<HistLine> {
    std::fs::read_to_string(hpath).unwrap_or_default().lines()
        .filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// Send allowlist. Source, first match wins:
///   1. ORANGE_SEND_ALLOWED_ROOMS env var: room ids separated by commas/whitespace, or `*` for any room;
///   2. `<state>/allowed_rooms` file: one room id per line, `#` comments allowed (`*` = any room).
/// If neither is configured, `send` refuses every room (safe default).
fn allowed_rooms(ctx: &Ctx) -> Vec<String> {
    if let Ok(v) = std::env::var("ORANGE_SEND_ALLOWED_ROOMS") {
        let list: Vec<String> = v.split(|c: char| c == ',' || c.is_whitespace())
            .map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
        if !list.is_empty() { return list; }
    }
    std::fs::read_to_string(ctx.state.join("allowed_rooms")).unwrap_or_default().lines()
        .map(|l| l.split('#').next().unwrap().trim().to_string()).filter(|l| !l.is_empty()).collect()
}

fn allowed(ctx: &Ctx, room: &str) -> bool {
    allowed_rooms(ctx).iter().any(|l| l == "*" || l == room)
}

fn default_state_dir() -> String {
    if let Ok(x) = std::env::var("XDG_STATE_HOME") { if !x.is_empty() { return format!("{x}/orange-send"); } }
    format!("{}/.local/state/orange-send", std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

const USAGE: &str = "usage: orange-send [--identity FILE] [--state DIR] <command>
  whoami                       print this identity's public Orange name
  rooms [--members]            read-only refresh of the room catalog; list room ids
  read ROOM [-n N] [--json]    catch up ROOM over the network, print the last N text messages (default 10)
  send ROOM [TEXT|-]           send TEXT (or stdin when '-' or omitted) to ROOM; prints JSON with timings
                               ROOM must be allowed via ORANGE_SEND_ALLOWED_ROOMS (comma list, or *)
                               or <state>/allowed_rooms (one id per line)
options/env: --identity FILE | ORANGE_IDENTITY_FILE (required, mode 600)
             --state DIR     | ORANGE_SEND_STATE (default $XDG_STATE_HOME/orange-send or ~/.local/state/orange-send)";

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    if let Err(e) = run().await { eprintln!("orange-send: {e}"); std::process::exit(1); }
}

async fn run() -> R<()> {
    let t0 = Instant::now();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut take_opt = |name: &str| -> Option<String> {
        let i = args.iter().position(|a| a == name)?;
        args.remove(i);
        (i < args.len()).then(|| args.remove(i))
    };
    let identity = take_opt("--identity").or_else(|| std::env::var("ORANGE_IDENTITY_FILE").ok())
        .ok_or("no identity: pass --identity FILE or set ORANGE_IDENTITY_FILE")?;
    let state: PathBuf = take_opt("--state").or_else(|| std::env::var("ORANGE_SEND_STATE").ok())
        .unwrap_or_else(default_state_dir).into();
    let n: usize = take_opt("-n").and_then(|v| v.parse().ok()).unwrap_or(10);
    let json = if let Some(i) = args.iter().position(|a| a == "--json") { args.remove(i); true } else { false };
    let members = if let Some(i) = args.iter().position(|a| a == "--members") { args.remove(i); true } else { false };
    let cmd = args.first().cloned().ok_or(USAGE)?;

    let ipath = Path::new(&identity);
    check_private(ipath)?;
    let secret: Secret = std::fs::read_to_string(ipath).ok()
        .and_then(|s| serde_json::from_str(s.trim()).ok()).ok_or("cannot parse identity file")?;
    if !secret.path().is_empty() || secret.name().to_string() != secret.public(None).public_key().to_string() {
        return Err("identity file is not a root Orange identity".into());
    }
    private_dir(&state)?;
    let me = secret.name();
    let mut ctx = Ctx { secret, me, state, resolver: DefaultResolver::default() };

    match cmd.as_str() {
        "whoami" => println!("{me}"),
        "rooms" => {
            let client = ctx.client().await?;
            let t1 = Instant::now();
            let (rooms, reads) = ctx.refresh_rooms(&client).await?;
            eprintln!("catalog: {} rooms, {reads} storage reads in {} ms", rooms.len(), ms(t1.elapsed()));
            for loc in rooms {
                let id = Id::hash(&loc).to_string();
                if members {
                    let mut inst = Instance::<ChatRoom>::receive(ctx.secret.clone(), loc).map_err(|e| format!("{e:?}"))?;
                    // read only the Init + first few actions (enough for DM membership)
                    let mut req = inst.start();
                    let mut names = String::new();
                    for _ in 0..4 {
                        let rx = client.send(req).await;
                        let Ok(Ok(resp)) = tokio::time::timeout(TIMEOUT, rx.recv()).await else { break };
                        if matches!(inst.response(&mut ctx.resolver, resp).await, Output::Subscribed) { break; }
                        match inst.request() { Some(r) => req = r, None => break }
                    }
                    if let Some(r) = inst.confirmed() {
                        names = r.members.iter().map(|m| if *m == me { "me".into() } else { m.to_string() })
                            .collect::<Vec<_>>().join(",");
                    }
                    println!("{id}\t{names}");
                } else { println!("{id}"); }
            }
        }
        "read" => {
            let room = args.get(1).ok_or(USAGE)?.clone();
            let client = ctx.client().await?;
            let t1 = Instant::now();
            let (mut run, _lock, ip, hp) = open_room(&mut ctx, &room).await?;
            run.drive(&mut ctx, &client, None).await?;
            persist(&run, &ip, &hp)?;
            let all = history(&hp);
            eprintln!("caught up: {} new text messages, {} round trips, {} undecoded (non-text) in {} ms; total {} ms",
                run.new.len(), run.reads, run.garbage, ms(t1.elapsed()), ms(t0.elapsed()));
            for l in &all[all.len().saturating_sub(n)..] {
                if json { println!("{}", serde_json::to_string(l).unwrap()); continue; }
                let when = chrono::DateTime::from_timestamp_nanos(l.ts_ns as i64).with_timezone(&chrono::Local);
                let who = if l.mine { "me".to_string() } else { l.author[..12].to_string() };
                println!("[{}] {who}: {}", when.format("%Y-%m-%d %H:%M:%S"), l.text);
            }
        }
        "send" => {
            let room = args.get(1).ok_or(USAGE)?.clone();
            if !allowed(&ctx, &room) { return Err(format!("room {room} is not allowed (set ORANGE_SEND_ALLOWED_ROOMS or add it to {}/allowed_rooms)", ctx.state.display())); }
            let text = match args.get(2).map(String::as_str) {
                Some("-") | None => { let mut s = String::new(); std::io::stdin().read_to_string(&mut s).map_err(|e| e.to_string())?; s.trim_end_matches('\n').to_string() }
                Some(t) => t.to_string(),
            };
            if text.trim().is_empty() { return Err("empty message".into()); }
            let client = ctx.client().await?;
            let t_conn = t0.elapsed();
            let (mut run, _lock, ip, hp) = open_room(&mut ctx, &room).await?;
            let t1 = Instant::now();
            let sent_at = run.drive(&mut ctx, &client, Some(text.clone())).await;
            persist(&run, &ip, &hp)?;   // persist cursor even on error
            let server_ts = sent_at?.ok_or("no Created confirmation")?;
            let mut out = BTreeMap::new();
            out.insert("ok", serde_json::json!(true));
            out.insert("room", serde_json::json!(room));
            out.insert("server_ts_ns", serde_json::json!(server_ts));
            out.insert("message_id", serde_json::json!(Id::hash(&(me, server_ts)).to_string()));
            out.insert("chars", serde_json::json!(text.chars().count()));
            out.insert("round_trips", serde_json::json!(run.reads));
            out.insert("other_new_messages", serde_json::json!(run.new.len().saturating_sub(1)));
            out.insert("connect_ms", serde_json::json!(ms(t_conn)));
            out.insert("catchup_and_write_ms", serde_json::json!(ms(t1.elapsed())));
            out.insert("total_ms", serde_json::json!(ms(t0.elapsed())));
            out.insert("server_ts_minus_start_ms", serde_json::json!((server_ts.saturating_sub(now_ns() - t0.elapsed().as_nanos() as u64)) as f64 / 1e6));
            println!("{}", serde_json::to_string(&out).unwrap());
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}
