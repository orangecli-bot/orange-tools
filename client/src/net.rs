//! One-shot network helpers for CLI commands (catalog refresh, room catch-up, send).
use crate::common::*;
use air::{DefaultResolver, Id, Identity, Instance, Location, Name, Output, Request, RequestType, Resolver, Response, Secret};
use air_server::Client;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const TIMEOUT: Duration = Duration::from_secs(20);

pub struct Ctx { pub secret: Secret, pub me: Name, pub state: PathBuf, pub resolver: DefaultResolver, client: Option<Client> }

#[derive(Serialize, Deserialize, Clone)]
pub struct HistLine { pub id: String, pub room: String, pub sender: String, pub mine: bool, pub timestamp_ns: u64, pub content: String }

pub struct RoomRun { pub inst: Instance<ChatRoom>, pub new: Vec<HistLine>, pub reads: usize, pub garbage: usize, pub room: String }

impl Ctx {
    pub fn new(secret: Secret, state: PathBuf) -> Self {
        let me = secret.name();
        Ctx { secret, me, state, resolver: DefaultResolver::default(), client: None }
    }
    fn cli_dir(&self) -> PathBuf { self.state.join("cli") }
    pub async fn client(&mut self) -> R<&Client> {
        if self.client.is_none() {
            let server: std::sync::Arc<Identity> = self.resolver.resolve(Name::orange_me(), None).await;
            let c = tokio::time::timeout(TIMEOUT, Client::new((*server).clone(), "wss")).await
                .map_err(|_| "connect timeout".to_string())??;
            self.client = Some(c);
        }
        Ok(self.client.as_ref().unwrap())
    }
    async fn roundtrip(&mut self, req: Request) -> R<Response> {
        let client = self.client().await?;
        let rx = client.send(req).await;
        tokio::time::timeout(TIMEOUT, rx.recv()).await.map_err(|_| "Air request timeout".to_string())?
            .map_err(|_| "Air response stream closed".to_string())
    }

    fn locations_path(&self) -> PathBuf { self.cli_dir().join("locations.bin") }
    pub fn cached_locations(&self) -> Vec<Location> {
        let mut out: Vec<Location> = std::fs::read(self.locations_path()).ok()
            .and_then(|b| postcard::from_bytes(&b).ok()).unwrap_or_default();
        // Also the listener's catalog (same identity, same state dir), if present.
        if let Some(cat) = std::fs::read(self.state.join("listen").join("catalog.bin")).ok()
            .and_then(|b| postcard::from_bytes::<air::Air>(&b).ok()) {
            for l in cat.list(&<ChatRoom as air::Contract>::id()) { if !out.contains(&l) { out.push(l); } }
        }
        out
    }

    /// Read-only catalog refresh: replays the identity's STORAGE channel (every room location this
    /// identity registered: kiwi and Orange both write it) from a fresh air::Air. Inbox channels are
    /// not started, and a fresh Air has an empty storage queue, so only Request::Read is ever sent.
    pub async fn refresh_rooms(&mut self) -> R<(Vec<Location>, usize)> {
        let mut catalog = air::Air::new(self.secret.clone()).map_err(|e| format!("{e:?}"))?;
        let mut request = catalog.start().remove(&RequestType::Storage).ok_or("no storage request")?;
        let mut reads = 0;
        loop {
            if !matches!(request, Request::Read(_)) { return Err("refusing: catalog wanted to write".into()); }
            let resp = self.roundtrip(request).await?;
            reads += 1;
            let done = matches!(resp, Response::Read(_, None));
            catalog.response(&mut self.resolver, RequestType::Storage, resp).await;
            if done { break; }
            request = match catalog.requests().remove(&RequestType::Storage) { Some(r) => r, None => break };
        }
        let mut rooms: Vec<Location> = catalog.list(&<ChatRoom as air::Contract>::id()).into_iter().collect();
        for l in self.cached_locations() { if !rooms.contains(&l) { rooms.push(l); } }
        private_dir(&self.cli_dir())?;
        write_private(&self.locations_path(), &postcard::to_allocvec(&rooms).unwrap())?;
        Ok((rooms, reads))
    }

    pub async fn find_room(&mut self, room: &str) -> R<Location> {
        if let Some(l) = self.cached_locations().into_iter().find(|l| Id::hash(l).to_string() == room) { return Ok(l); }
        let (rooms, _) = self.refresh_rooms().await?;
        rooms.into_iter().find(|l| Id::hash(l).to_string() == room)
            .ok_or_else(|| format!("room {room} is not in this identity's room list (not a member, or wrong id)"))
    }

    fn room_paths(&self, id: &str) -> (PathBuf, PathBuf, PathBuf) {
        let d = self.cli_dir().join("rooms");
        (d.join(format!("{id}.instance")), d.join(format!("{id}.jsonl")), d.join(format!("{id}.lock")))
    }

    pub async fn open_room(&mut self, room: &str) -> R<(RoomRun, Lock, PathBuf, PathBuf)> {
        if room.len() != 64 || !room.bytes().all(|b| b.is_ascii_hexdigit()) { return Err(format!("bad room id {room:?}")); }
        private_dir(&self.cli_dir().join("rooms"))?;
        let (ipath, hpath, lpath) = self.room_paths(room);
        let lock = Lock::take(&lpath)?;
        let mut cached = std::fs::read(&ipath).ok().and_then(|b| postcard::from_bytes::<Instance<ChatRoom>>(&b).ok());
        if cached.is_none() {
            // Seed from the running listener's caught-up room (same identity/state dir): instance first,
            // then its history (history is always >= the instance cursor; duplicates are dropped by id).
            let ldir = self.state.join("listen").join("rooms");
            if let Some(i) = std::fs::read(ldir.join(format!("{room}.instance"))).ok().and_then(|b| postcard::from_bytes::<Instance<ChatRoom>>(&b).ok()) {
                let hist = std::fs::read_to_string(ldir.join(format!("{room}.jsonl"))).unwrap_or_default();
                write_private(&hpath, hist.as_bytes())?;
                cached = Some(i);
            }
        }
        let inst = match cached {
            Some(i) => i,
            None => {
                let loc = self.find_room(room).await?;
                Instance::<ChatRoom>::receive(self.secret.clone(), loc).map_err(|e| format!("{e:?}"))?
            }
        };
        Ok((RoomRun { inst, new: vec![], reads: 0, garbage: 0, room: room.to_string() }, lock, ipath, hpath))
    }

    /// Catch the room up to its head; if `outgoing` is set, write it after catching up and wait
    /// for the server's Created. Returns the server timestamp of our write.
    pub async fn drive(&mut self, run: &mut RoomRun, outgoing: Option<String>, force: bool) -> R<Option<u64>> {
        let mut outgoing = outgoing;
        let mut sending = false;
        let mut request = run.inst.start();
        loop {
            let resp = self.roundtrip(request).await?;
            run.reads += 1;
            match run.inst.response(&mut self.resolver, resp).await {
                Output::Subscribed => {
                    if let Some(text) = outgoing.take() {
                        let room = run.inst.confirmed().ok_or("room has no Init yet (wrong location?)")?;
                        if !room.members.contains(&self.me) && !force {
                            return Err("identity is not in this room's decoded member list; refusing to send (use --force for Orange group rooms whose membership uses variants this client does not decode)".into());
                        }
                        run.inst.send(ChatRoomAction::SendMessage(text));
                        sending = true;
                    } else if !sending { return Ok(None); }
                }
                Output::Created(r, _) => {
                    if let Some(t) = r { record(run, self.me, &t); }
                    if sending { return Ok(run.new.last().map(|l| l.timestamp_ns)); }
                }
                Output::Read(r, _) => { if let Some(t) = r { record(run, self.me, &t); } }
                Output::Init(_) => {}
                Output::Garbage => run.garbage += 1,
            }
            request = match run.inst.request() { Some(r) => r, None => return Err("channel idle unexpectedly".into()) };
        }
    }
}

fn record(run: &mut RoomRun, me: Name, t: &Text) {
    run.new.push(HistLine { id: message_id(t.author, t.timestamp), room: run.room.clone(), sender: t.author.to_string(),
        mine: t.author == me, timestamp_ns: t.timestamp, content: t.text.clone() });
}

pub fn persist(run: &RoomRun, ipath: &Path, hpath: &Path) -> R<()> {
    write_private(ipath, &postcard::to_allocvec(&run.inst).unwrap())?;
    if !run.new.is_empty() {
        let mut s = String::new();
        for l in &run.new { s.push_str(&serde_json::to_string(l).unwrap()); s.push('\n'); }
        append_private(hpath, &s)?;
    }
    Ok(())
}

pub fn history(hpath: &Path) -> Vec<HistLine> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<HistLine> = std::fs::read_to_string(hpath).unwrap_or_default().lines()
        .filter_map(|l| serde_json::from_str::<HistLine>(l).ok()).filter(|h| seen.insert(h.id.clone())).collect();
    out.sort_by_key(|h| h.timestamp_ns);
    out
}
