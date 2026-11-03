//! air-client: generic headless Air client for one identity per config (Orange or kiwi identity).
mod common;
mod listen;
mod net;
mod wake;

use common::*;
use net::*;
use std::collections::{BTreeMap, HashMap};
use std::io::Read as _;
use std::time::Instant;

const USAGE: &str = "usage: air-client [--config FILE] [--identity FILE] [--state DIR] <command>
  whoami [--json]                  print this identity's Air name (public key)
  rooms [--json] [--members]       read-only refresh of the room list (storage channel); list room ids
  read ROOM [-n N] [--json]        catch ROOM up over the network, print the last N text messages (default 10)
  send ROOM [TEXT|-] [--request-id ID] [--force]
                                   send TEXT (stdin when '-' or omitted); ROOM must be one of this identity's rooms
                                   and the identity must be a decoded member. Prints kiwi-style {\"ok\":true,\"result\":{...}}
  unread [--json]                  print new messages recorded by `listen` and mark them read (air_unread.sh)
  call JSON|-                      kiwi-compatible JSON API subset: identity, list, get, messages, send
  listen                           long-lived listener: unread.jsonl + optional batched wake webhook
config: env file (mode 600) via --config or AIR_CLIENT_CONFIG; keys AIR_IDENTITY_FILE, AIR_STATE_DIR, AIR_NOTIFY_ENV,
        AIR_WAKE_FORMAT, AIR_WAKE_BATCH_MS, AIR_UNREAD_FILE, AIR_READ_FILE, AIR_CONTACTS_FILE, AIR_IMPORT_SEEN,
        AIR_START_AFTER_NS (see INSTALL.md)";

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let is_call = args.iter().any(|a| a == "call");
    let is_send = args.iter().any(|a| a == "send");
    if let Err(e) = run(&mut args).await {
        if is_call || is_send { println!("{}", serde_json::json!({"ok": false, "error": e})); std::process::exit(2); }
        eprintln!("air-client: {e}");
        std::process::exit(1);
    }
}

fn take_opt(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.remove(i);
    (i < args.len()).then(|| args.remove(i))
}
fn take_flag(args: &mut Vec<String>, name: &str) -> bool {
    if let Some(i) = args.iter().position(|a| a == name) { args.remove(i); true } else { false }
}
fn read_stdin() -> R<String> {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).map_err(|e| e.to_string())?;
    Ok(s)
}

async fn run(args: &mut Vec<String>) -> R<()> {
    let t0 = Instant::now();
    if args.iter().any(|a| a == "-h" || a == "--help" || a == "help") || args.is_empty() { println!("{USAGE}"); return Ok(()); }
    let mut flags: HashMap<&'static str, String> = HashMap::new();
    for k in ["--config", "--identity", "--state"] { if let Some(v) = take_opt(args, k) { flags.insert(k, v); } }
    let n: usize = take_opt(args, "-n").and_then(|v| v.parse().ok()).unwrap_or(10);
    let request_id = take_opt(args, "--request-id");
    let json = take_flag(args, "--json");
    let members = take_flag(args, "--members");
    let force = take_flag(args, "--force");
    let cmd = args.first().cloned().ok_or(USAGE)?;
    let cfg = load_config(&flags)?;

    if cmd == "listen" { return listen::listen(cfg).await; }
    if cmd == "unread" { return unread(&cfg, json); }

    let (secret, leaks) = load_identity(&cfg.identity_file)?;
    private_dir(&cfg.state_dir)?;
    let me = secret.name();
    let mut ctx = Ctx::new(secret, cfg.state_dir.clone());
    let contacts = load_contacts(cfg.contacts_file.as_deref());

    match cmd.as_str() {
        "whoami" => {
            if json { println!("{}", serde_json::json!({"ok": true, "name": me.to_string()})); } else { println!("{me}"); }
        }
        "rooms" => {
            let t1 = Instant::now();
            let (rooms, reads) = ctx.refresh_rooms().await?;
            eprintln!("rooms: {} rooms, {reads} storage reads in {} ms", rooms.len(), ms(t1.elapsed()));
            let mut out = vec![];
            for loc in rooms {
                let id = air::Id::hash(&loc).to_string();
                let mut names: Vec<String> = vec![];
                if members {
                    let (mut run, _l, ip, hp) = ctx.open_room(&id).await?;
                    ctx.drive(&mut run, None, false).await?;
                    persist(&run, &ip, &hp)?;
                    if let Some(r) = run.inst.confirmed() { names = r.members.iter().map(|m| m.to_string()).collect(); }
                }
                if json { out.push(serde_json::json!({"id": id, "members": if members { serde_json::json!(names) } else { serde_json::Value::Null }})); }
                else if members {
                    println!("{id}\t{}", names.iter().map(|m| if *m == me.to_string() { "me".to_string() } else { m.clone() }).collect::<Vec<_>>().join(","));
                } else { println!("{id}"); }
            }
            if json { println!("{}", serde_json::json!(out)); }
        }
        "read" => {
            let room = args.get(1).ok_or(USAGE)?.clone();
            let t1 = Instant::now();
            let (mut run, _lock, ip, hp) = ctx.open_room(&room).await?;
            ctx.drive(&mut run, None, false).await?;
            persist(&run, &ip, &hp)?;
            eprintln!("caught up: {} new text messages, {} round trips, {} undecoded (non-text) in {} ms; total {} ms",
                run.new.len(), run.reads, run.garbage, ms(t1.elapsed()), ms(t0.elapsed()));
            let all = history(&hp);
            for l in &all[all.len().saturating_sub(n)..] {
                let content = if contains_leak(&l.content, &leaks) { "<redacted: contained private key material>".to_string() } else { l.content.clone() };
                let from = if l.mine { "me".to_string() } else { contacts.get(&l.sender).cloned().unwrap_or_else(|| "unknown".into()) };
                if json {
                    println!("{}", serde_json::json!({"id": l.id, "room": l.room, "from": from, "sender": l.sender, "mine": l.mine, "timestamp_ns": l.timestamp_ns, "content": content}));
                } else {
                    let when = chrono::DateTime::from_timestamp_nanos(l.timestamp_ns as i64).with_timezone(&chrono::Local);
                    let who = if l.mine { "me".to_string() } else if from != "unknown" { from } else { l.sender[..12].to_string() };
                    println!("[{}] {who}: {content}", when.format("%Y-%m-%d %H:%M:%S %Z"));
                }
            }
        }
        "send" => {
            let room = args.get(1).ok_or(USAGE)?.clone();
            let text = match args.get(2).map(String::as_str) {
                Some("-") | None => read_stdin()?.trim_end_matches('\n').to_string(),
                Some(t) => t.to_string(),
            };
            let out = do_send(&mut ctx, &cfg, &room, &text, request_id, force, &leaks).await?;
            println!("{out}");
        }
        "call" => {
            let raw = match args.get(1).map(String::as_str) { Some("-") | None => read_stdin()?, Some(s) => s.to_string() };
            let req: serde_json::Value = serde_json::from_str(raw.trim()).map_err(|e| format!("bad JSON request: {e}"))?;
            let out = call(&mut ctx, &cfg, &req, &leaks).await?;
            println!("{out}");
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

/// Print new messages and mark them read: same contract as air_unread.sh (prints the raw JSON lines
/// of unread.jsonl, or "no new messages"; moves them to read.jsonl). Locked against `listen`.
fn unread(cfg: &Config, json: bool) -> R<()> {
    let _lock = Lock::take(&cfg.unread_file.with_extension("lock"))?;
    let text = std::fs::read_to_string(&cfg.unread_file).unwrap_or_default();
    if text.trim().is_empty() {
        if json { println!("[]"); } else { println!("no new messages"); }
        return Ok(());
    }
    if json {
        let v: Vec<serde_json::Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        println!("{}", serde_json::json!(v));
    } else { print!("{text}"); if !text.ends_with('\n') { println!(); } }
    use std::io::Write as _;
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let block = if text.ends_with('\n') { text.clone() } else { format!("{text}\n") };
    append_private(&cfg.read_file, &block)?;
    write_private(&cfg.unread_file, b"")?;
    Ok(())
}

fn receipts_path(cfg: &Config) -> std::path::PathBuf { cfg.state_dir.join("cli").join("receipts.jsonl") }

async fn do_send(ctx: &mut Ctx, cfg: &Config, room: &str, text: &str, request_id: Option<String>, force: bool, leaks: &[String]) -> R<serde_json::Value> {
    let t0 = Instant::now();
    if text.trim().is_empty() { return Err("empty message".into()); }
    if contains_leak(text, leaks) { return Err("refusing to send: text contains this identity's private key material".into()); }
    private_dir(&cfg.state_dir.join("cli"))?;
    let _rlock = Lock::take(&cfg.state_dir.join("cli").join("receipts.lock"))?;
    // request_id: same idempotency contract as kiwi (reuse it when retrying; never double-sends).
    if let Some(rid) = &request_id {
        for line in std::fs::read_to_string(receipts_path(cfg)).unwrap_or_default().lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if v["request_id"] == *rid {
                if v["room"] != room || v["text_sha"] != text_sha(text) { return Err("request_id already used for different arguments".into()); }
                return Ok(serde_json::json!({"ok": true, "result": v["result"], "duplicate": true}));
            }
        }
    }
    let (mut run, _lock, ip, hp) = ctx.open_room(room).await?;
    let t1 = Instant::now();
    let sent = ctx.drive(&mut run, Some(text.to_string()), force).await;
    persist(&run, &ip, &hp)?;
    let server_ts = sent?.ok_or("no Created confirmation")?;
    let mut result = BTreeMap::new();
    result.insert("id", serde_json::json!(room));
    result.insert("status", serde_json::json!("sent"));
    result.insert("message_id", serde_json::json!(message_id(ctx.me, server_ts)));
    result.insert("timestamp_ns", serde_json::json!(server_ts));
    result.insert("round_trips", serde_json::json!(run.reads));
    result.insert("catchup_and_write_ms", serde_json::json!(ms(t1.elapsed())));
    result.insert("total_ms", serde_json::json!(ms(t0.elapsed())));
    let result = serde_json::json!(result);
    if let Some(rid) = request_id {
        append_private(&receipts_path(cfg), &format!("{}\n", serde_json::json!({"request_id": rid, "room": room, "text_sha": text_sha(text), "result": result})))?;
    }
    Ok(serde_json::json!({"ok": true, "result": result}))
}

fn text_sha(t: &str) -> String { air::Id::hash(&t.to_string()).to_string() }

/// kiwi's message JSON: {body:{Text}, author, timestamp, hidden_from, id}.
fn kiwi_msg(l: &HistLine, leaks: &[String]) -> serde_json::Value {
    let text = if contains_leak(&l.content, leaks) { "<redacted: contained private key material>".to_string() } else { l.content.clone() };
    serde_json::json!({"body": {"Text": text}, "author": l.sender, "timestamp": l.timestamp_ns, "hidden_from": [], "id": l.id})
}

async fn room_view(ctx: &mut Ctx, room: &str, leaks: &[String], tail: Option<usize>) -> R<serde_json::Value> {
    let (mut run, _lock, ip, hp) = ctx.open_room(room).await?;
    ctx.drive(&mut run, None, false).await?;
    persist(&run, &ip, &hp)?;
    let Some(c) = run.inst.confirmed().cloned() else {
        return Ok(serde_json::json!({"id": room, "contract": "ChatRoom", "confirmed": null, "pending": null}));
    };
    let hist = history(&hp);
    let start = tail.map(|t| hist.len().saturating_sub(t)).unwrap_or(0);
    let msgs: Vec<_> = hist[start..].iter().map(|l| kiwi_msg(l, leaks)).collect();
    let confirmed = serde_json::json!({"members": c.members.iter().map(|m| m.to_string()).collect::<Vec<_>>(), "messages": msgs, "id": c.id.to_string()});
    Ok(serde_json::json!({"id": room, "contract": "ChatRoom", "confirmed": confirmed, "pending": confirmed}))
}

async fn call(ctx: &mut Ctx, cfg: &Config, req: &serde_json::Value, leaks: &[String]) -> R<serde_json::Value> {
    let s = |k: &str| req[k].as_str().map(String::from);
    let ok = |v: serde_json::Value| Ok(serde_json::json!({"ok": true, "result": v}));
    match req["op"].as_str().unwrap_or("") {
        "identity" => ok(serde_json::json!({"name": ctx.me.to_string()})),
        "list" => {
            if !req["contract"].is_null() && req["contract"] != "ChatRoom" { return ok(serde_json::json!([])); }
            let (rooms, _) = ctx.refresh_rooms().await?;
            let mut out = vec![];
            for loc in rooms { out.push(room_view(ctx, &air::Id::hash(&loc).to_string(), leaks, None).await?); }
            ok(serde_json::json!(out))
        }
        "get" => {
            let id = s("id").ok_or("missing string id")?;
            let tail = req["tail"].as_u64().or(req["limit"].as_u64()).map(|n| n as usize);
            let mut v = room_view(ctx, &id, leaks, tail).await?;
            if tail.is_some() || req["pending"] == false { v.as_object_mut().unwrap().remove("pending"); }
            ok(v)
        }
        "messages" => {
            let id = s("id").ok_or("missing string id")?;
            let v = room_view(ctx, &id, leaks, None).await?;
            let msgs = v["confirmed"]["messages"].as_array().cloned().ok_or("not a confirmed chat room")?;
            let limit = req["limit"].as_u64().unwrap_or(100).min(1000) as usize;
            let start = match req["after"].as_str().filter(|x| !x.is_empty()) {
                Some(after) => msgs.iter().position(|m| m["id"].as_str() == Some(after)).map(|i| i + 1).ok_or("unknown message id in after")?,
                None if req["tail"] == true => msgs.len().saturating_sub(limit),
                None => 0,
            };
            let page: Vec<_> = msgs[start..].iter().take(limit).cloned().collect();
            let more = msgs.len() > start + page.len();
            ok(serde_json::json!({"id": id, "messages": page, "more": more, "total": msgs.len()}))
        }
        "send" => {
            let id = s("id").ok_or("missing string id")?;
            let text = s("text").ok_or("missing string text")?;
            let rid = s("request_id").filter(|r| !r.is_empty()).ok_or("mutations require a stable request_id; reuse it when retrying")?;
            do_send(ctx, cfg, &id, &text, Some(rid), req["force"] == true, leaks).await
        }
        op => Err(format!("op {op:?} not supported by air-client (supported: identity, list, get, messages, send)")),
    }
}
