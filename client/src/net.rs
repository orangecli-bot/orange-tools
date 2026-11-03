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
            // Up to 3 attempts (250 ms, 1 s backoff): a single websocket handshake can time out.
            let mut last = String::new();
            for (i, wait) in [0u64, 250, 1000].into_iter().enumerate() {
                tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                match tokio::time::timeout(TIMEOUT, Client::new((*server).clone(), "wss")).await {
                    Ok(Ok(c)) => { self.client = Some(c); break; }
                    Ok(Err(e)) => last = format!("connect attempt {}: {e}", i + 1),
                    Err(_) => last = format!("connect attempt {}: timeout", i + 1),
                }
            }
            if self.client.is_none() { return Err(last); }
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

// ---- room creation -------------------------------------------------------------------------------
impl Ctx {
    /// Decoded members of every room in this identity's room list (catches each room up; rooms the
    /// listener already follows are seeded from its caught-up copy, so this is cheap when it runs).
    pub async fn room_members(&mut self) -> R<Vec<(String, Vec<Name>, usize)>> {
        let (rooms, _) = self.refresh_rooms().await?;
        let mut out = vec![];
        for loc in rooms {
            let id = Id::hash(&loc).to_string();
            let (mut run, _l, ip, hp) = self.open_room(&id).await?;
            self.drive(&mut run, None, false).await?;
            persist(&run, &ip, &hp)?;
            let members = run.inst.confirmed().map(|c| c.members.clone()).unwrap_or_default();
            out.push((id, members, history(&hp).len()));
        }
        Ok(out)
    }

    /// Sequentially drive the given Air channels until none of them wants another request.
    async fn drive_air(&mut self, air: &mut air::Air, wanted: &dyn Fn(&RequestType) -> bool) -> R<usize> {
        let mut trips = 0;
        loop {
            let reqs: Vec<(RequestType, Request)> = air.requests().into_iter().filter(|(k, _)| wanted(k)).collect();
            if reqs.is_empty() { return Ok(trips); }
            for (kind, req) in reqs {
                let resp = self.roundtrip(req).await?;
                trips += 1;
                air.response(&mut self.resolver, kind, resp).await;
                if trips > 20000 { return Err("Air channel sync did not converge".into()); }
            }
        }
    }

    /// Create a ChatRoom with `recipients` exactly like Orange 1.0.1 / kiwi do for DMs:
    /// init = Id::hash(&recipient) (deterministic per creator), Share(recipient) action, location
    /// registered in our STORAGE channel and delivered to the recipient's inbox (shared-key channel +
    /// notification). Groups (2+ recipients) use kiwi's form: random init + Share per member.
    /// For a DM, an existing room whose decoded members are exactly {me, recipient} is returned
    /// instead (Orange's rule: prefer one with messages, then the lowest room id).
    pub async fn create_room(&mut self, members: Vec<Name>) -> R<(String, &'static str, usize)> {
        let me = self.me;
        let mut recipients: Vec<Name> = members.into_iter().filter(|n| *n != me).collect();
        recipients.sort_unstable();
        recipients.dedup();
        if recipients.is_empty() { return Err("at least one recipient other than this identity is required".into()); }
        if recipients.len() == 1 {
            let r = recipients[0];
            let mut existing: Vec<(bool, String)> = self.room_members().await?.into_iter()
                .filter(|(_, m, _)| m.contains(&me) && m.contains(&r) && m.iter().all(|n| *n == me || *n == r))
                .map(|(id, _, n)| (n == 0, id)).collect();
            existing.sort();
            if let Some((_, id)) = existing.into_iter().next() { return Ok((id, "existing", 0)); }
        }
        // Start from the listener's catalog when present (cursors near the head), else fresh.
        let mut air = std::fs::read(self.state.join("listen").join("catalog.bin")).ok()
            .and_then(|b| postcard::from_bytes::<air::Air>(&b).ok())
            .map(Ok).unwrap_or_else(|| air::Air::new(self.secret.clone()).map_err(|e| format!("{e:?}")))?;
        // Phase 1: storage caught up first, so we never re-register a location that is already stored.
        let _ = air.start();
        let mut trips = self.drive_air(&mut air, &|k| *k == RequestType::Storage).await?;
        let init = if recipients.len() == 1 { Id::hash(&recipients[0]) } else { Id::random() };
        let mut inst: Instance<ChatRoom> = air.create(init).map_err(|e| format!("{e:?}"))?;
        let location = *inst.location();
        let id = Id::hash(&location).to_string();
        if self.cached_locations().iter().any(|l| Id::hash(l).to_string() == id) {
            return Ok((id, "existing", trips));
        }
        for r in &recipients {
            air.share(&mut self.resolver, *r, location).await;
            inst.send(ChatRoomAction::Share(*r));
        }
        // Phase 2: write the room channel (Init + Shares) and wait until it's confirmed.
        let mut run = RoomRun { inst, new: vec![], reads: 0, garbage: 0, room: id.clone() };
        let mut req = run.inst.start();
        loop {
            let resp = self.roundtrip(req).await?;
            run.reads += 1;
            let out = run.inst.response(&mut self.resolver, resp).await;
            if let Output::Read(Some(t), _) | Output::Created(Some(t), _) = &out { record(&mut run, self.me, t); }
            match run.inst.request() { Some(r) => req = r, None => break }
            if matches!(out, Output::Subscribed) && run.inst.confirmed().is_some_and(|c| recipients.iter().all(|r| c.members.contains(r))) { break; }
            if run.reads > 1000 { return Err("room write did not converge".into()); }
        }
        trips += run.reads;
        // Phase 3: register in STORAGE and deliver to each recipient's inbox (+ notification).
        let rs = recipients.clone();
        trips += self.drive_air(&mut air, &move |k| match k {
            RequestType::Storage => true,
            RequestType::Inbox(air::inbox::RequestType::Channel(n)) | RequestType::Inbox(air::inbox::RequestType::Notifying(n)) => rs.contains(n),
            RequestType::Inbox(air::inbox::RequestType::Notified) => true,
            _ => false,
        }).await?;
        // Cache the new room for later CLI read/send.
        private_dir(&self.cli_dir().join("rooms"))?;
        let (ip, hp, _) = self.room_paths(&id);
        persist(&run, &ip, &hp)?;
        let mut locs: Vec<Location> = std::fs::read(self.locations_path()).ok().and_then(|b| postcard::from_bytes(&b).ok()).unwrap_or_default();
        if !locs.contains(&location) { locs.push(location); }
        write_private(&self.locations_path(), &postcard::to_allocvec(&locs).unwrap())?;
        Ok((id, "created", trips))
    }
}
