//! Shared pieces: the ChatRoom contract mirror, config/identity loading and private-file helpers.
use air::{Contract, Id, Metadata, Name, Secret};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub type R<T> = Result<T, String>;

// ---- chk ChatRoom contract, wire-compatible subset ---------------------------------------------
// Contract id "ChatRoom", Init = Id, ChatRoomAction variants 0 (Share) and 1 (SendMessage) at the
// same positions/types as chk (both kiwi's chk e2a15c6 and Orange 1.0.1's chk 2da2ba1), so postcard
// bytes and the signed Hash are identical. Other actions (stickers, GIFs, photos, video, delete,
// Orange group ops) do not decode -> Output::Garbage; the channel cursor still advances.
#[derive(Debug, Clone, Hash, Serialize, Deserialize)]
pub enum ChatRoomAction { Share(Name), SendMessage(String) }

#[derive(Debug, Clone, Hash, Serialize, Deserialize)]
pub struct ChatRoom { pub members: Vec<Name>, pub id: Id }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Text { pub author: Name, pub timestamp: u64, pub text: String }

impl Contract for ChatRoom {
    type Init = Id;
    type Message = ChatRoomAction;
    type Result = Option<Text>;
    fn id() -> Id { Id::hash("ChatRoom") }
    fn init(init: Id, m: Metadata) -> Self { ChatRoom { members: vec![m.signer], id: init } }
    fn apply(&mut self, message: ChatRoomAction, m: Metadata) -> Option<Text> {
        match message {
            ChatRoomAction::Share(n) => { if !self.members.contains(&n) { self.members.push(n); } None }
            ChatRoomAction::SendMessage(text) => Some(Text { author: m.signer, timestamp: m.timestamp, text }),
        }
    }
}

/// chk's message id: Id::hash(&(signer, timestamp)) (same in kiwi and Orange).
pub fn message_id(author: Name, ts: u64) -> String { Id::hash(&(author, ts)).to_string() }

// ---- files ---------------------------------------------------------------------------------------
pub fn private_dir(p: &Path) -> R<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("{}: {e}", p.display()))
}
pub fn check_private(p: &Path) -> R<()> {
    use std::os::unix::fs::PermissionsExt;
    let m = std::fs::metadata(p).map_err(|e| format!("{}: {e}", p.display()))?;
    if m.permissions().mode() & 0o077 != 0 { return Err(format!("{} must not be group/other accessible (chmod 600)", p.display())); }
    Ok(())
}
pub fn write_private(p: &Path, bytes: &[u8]) -> R<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600)
        .open(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    f.write_all(bytes).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, p).map_err(|e| e.to_string())
}
pub fn append_private(p: &Path, text: &str) -> R<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(p)
        .map_err(|e| format!("{}: {e}", p.display()))?;
    f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())
}

/// Exclusive flock held for the lifetime of the value.
pub struct Lock(#[allow(dead_code)] std::fs::File);
impl Lock {
    pub fn take(p: &Path) -> R<Self> {
        use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
        let f = std::fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(p)
            .map_err(|e| format!("{}: {e}", p.display()))?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 { return Err("flock failed".into()); }
        Ok(Lock(f))
    }
    pub fn try_take(p: &Path) -> R<Option<Self>> {
        use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
        let f = std::fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(p)
            .map_err(|e| format!("{}: {e}", p.display()))?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 { return Ok(None); }
        Ok(Some(Lock(f)))
    }
}

pub fn parse_env_file(path: &Path) -> R<HashMap<String, String>> {
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
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\''))).unwrap_or(v);
            map.insert(k.trim().to_string(), v.to_string());
        }
    }
    Ok(map)
}

// ---- config ------------------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WakeFormat { Kiwi, Orange }

pub struct Config {
    pub identity_file: PathBuf,
    pub state_dir: PathBuf,
    pub unread_file: PathBuf,
    pub read_file: PathBuf,
    pub contacts_file: Option<PathBuf>,
    pub notify_env: Option<PathBuf>,
    pub wake_format: WakeFormat,
    pub wake_batch_ms: u64,
    pub wake_source: String,
    pub import_seen: Option<PathBuf>,
    pub start_after_ns: Option<u64>,
}

/// Config precedence: command-line flag > process env > config file (--config / AIR_CLIENT_CONFIG).
pub fn load_config(flags: &HashMap<&'static str, String>) -> R<Config> {
    let mut file = HashMap::new();
    if let Some(p) = flags.get("--config").cloned().or_else(|| std::env::var("AIR_CLIENT_CONFIG").ok()).filter(|s| !s.is_empty()) {
        file = parse_env_file(Path::new(&p))?;
    }
    let get = |k: &str| -> Option<String> {
        std::env::var(k).ok().filter(|v| !v.is_empty()).or_else(|| file.get(k).filter(|v| !v.is_empty()).cloned())
    };
    let identity_file: PathBuf = flags.get("--identity").cloned().or_else(|| get("AIR_IDENTITY_FILE"))
        .ok_or("AIR_IDENTITY_FILE not set (use --config FILE, AIR_CLIENT_CONFIG, or --identity)")?.into();
    let state_dir: PathBuf = flags.get("--state").cloned().or_else(|| get("AIR_STATE_DIR"))
        .ok_or("AIR_STATE_DIR not set (use --config FILE, AIR_CLIENT_CONFIG, or --state)")?.into();
    let unread_file: PathBuf = get("AIR_UNREAD_FILE").map(PathBuf::from).unwrap_or_else(|| state_dir.join("unread.jsonl"));
    let read_file: PathBuf = get("AIR_READ_FILE").map(PathBuf::from)
        .unwrap_or_else(|| unread_file.with_file_name("read.jsonl"));
    let wake_format = match get("AIR_WAKE_FORMAT").as_deref() {
        None | Some("kiwi") | Some("air_message") => WakeFormat::Kiwi,
        Some("orange") | Some("orange_message") => WakeFormat::Orange,
        Some(o) => return Err(format!("AIR_WAKE_FORMAT={o}: expected kiwi or orange")),
    };
    Ok(Config {
        identity_file, state_dir, unread_file, read_file,
        contacts_file: get("AIR_CONTACTS_FILE").map(PathBuf::from),
        notify_env: get("AIR_NOTIFY_ENV").map(PathBuf::from),
        wake_format,
        wake_batch_ms: get("AIR_WAKE_BATCH_MS").and_then(|v| v.parse().ok()).unwrap_or(1500),
        wake_source: get("AIR_WAKE_SOURCE").unwrap_or_else(|| "air-client".into()),
        import_seen: get("AIR_IMPORT_SEEN").map(PathBuf::from),
        start_after_ns: get("AIR_START_AFTER_NS").and_then(|v| v.parse().ok()),
    })
}

/// Loads an identity from either a plain secret JSON ({"name","temporary","path"}: Orange's
/// `ramp.air.identity` / Bob-style secret.json) or a kiwi data-dir state.json (takes `.secret`).
/// Returns the secret plus lowercase strings that must never appear in output (leak filter).
pub fn load_identity(path: &Path) -> R<(Secret, Vec<String>)> {
    check_private(path)?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(text.trim()).map_err(|_| format!("{}: not JSON", path.display()))?;
    let secret_value = if value.get("secret").is_some() && value.get("version").is_some() { value["secret"].clone() } else { value };
    let mut leaks = vec![];
    if let Some(t) = secret_value.get("temporary").and_then(|t| t.as_str()) {
        if t.len() >= 16 { leaks.push(t.to_ascii_lowercase()); }
    }
    let secret: Secret = serde_json::from_value(secret_value).map_err(|_| format!("{}: not an Air secret", path.display()))?;
    // Same validity check Orange applies when loading "ramp.air.identity" (ramp2 lib.rs:35-38).
    if !secret.path().is_empty() || secret.name().to_string() != secret.public(None).public_key().to_string() {
        return Err("identity file does not contain a root Air identity".into());
    }
    Ok((secret, leaks))
}

pub fn contains_leak(text: &str, leaks: &[String]) -> bool {
    if leaks.is_empty() { return false; }
    let t = text.to_ascii_lowercase();
    leaks.iter().any(|l| t.contains(l))
}

/// contacts.json: {"label": {"air_name": "<hex>", ...}, ...} -> name -> label (kiwi_daemon's `from`).
pub fn load_contacts(path: Option<&Path>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(p) = path else { return out };
    let Ok(text) = std::fs::read_to_string(p) else { return out };
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&text) else { return out };
    for (label, v) in map {
        if let Some(n) = v.get("air_name").and_then(|n| n.as_str()) { out.entry(n.to_string()).or_insert(label); }
    }
    out
}

pub fn now_ns() -> u64 { chrono::Utc::now().timestamp_nanos_opt().unwrap() as u64 }
pub fn ms(d: std::time::Duration) -> f64 { (d.as_secs_f64() * 10000.0).round() / 10.0 }

/// One unread line, exactly kiwi_daemon.sh's shape: {from, sender, room, timestamp_ns, content}.
pub fn unread_line(from: &str, sender: &str, room: &str, ts: u64, content: &str) -> serde_json::Value {
    serde_json::json!({"from": from, "sender": sender, "room": room, "timestamp_ns": ts, "content": content})
}
