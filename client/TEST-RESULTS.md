# air-client test results (2026-11-03)

Binary built with `cargo build --release` (Rust 1.99.1, air 4.0.1, air_server 2.0.1; Cargo.lock = Orange 1.0.1's lock + serde_json preserve_order).
glibc: the highest symbol version needed is GLIBC_2.34. Links only libc, libm, libgcc_s. Listener RSS was about 8 MB.
All test identities, state, configs and the local webhook receiver lived in a scratch directory outside the source tree.
kiwi for the tests: a local kiwi build (air 3.0.0 + chk e2a15c6), run only with fresh `--data-dir`s.
No real person or other bot was messaged. Sends went only to throwaway rooms.
Ids below are placeholders: `<ROOM_ID>`, `<NAME_A>`, `<NAME_B>`, `<MSG_ID_1>`, `<MSG_ID_2>`.

## Q: Does a kiwi (air 3.0.0) identity keep its name on air 4.0.1 / Orange? **Yes. Tested.**
Source analysis (air-3.0.0 vs air-4.0.1 crates):
- `Secret` serde is identical (`{name, temporary, path}`), and so are name derivation (`AIR_NAMES` tagged hash, secp256k1 pubkey), signing, channel key derivation and encryption.
  The 3.0.0 to 4.0.1 diff in names.rs/channel.rs/contract.rs is formatting plus additive APIs.
- Room location, room id (`Id::hash(location)`), the `Signed<Message<C>>` envelope and slot encryption are unchanged.
- air 3's `Inbox` keeps the STORAGE channel as its self-channel, with the same key derivation as air 4's `Air.storage` (`set_path([INBOX]).harden().derive([STORAGE])`).
  The notified/notifications/shared-key inbox channels use byte-for-byte the same derivation, so air 4 finds every room kiwi registered or received.
- kiwi's chk (e2a15c6) and Orange's chk (2da2ba1) both have `ChatRoomAction::Share`=0 and `SendMessage`=1, contract id `ChatRoom`, Init `Id`, and message id `Id::hash(&(signer, timestamp))`.

Empirical test with two throwaway kiwi identities (A created a DM room with B using kiwi 0.1.0):
| step | command | result |
|---|---|---|
| kiwi init A, B; `kiwi call room` (A→B), `kiwi call send` A→B #1, B→A #2 | kiwi | room `<ROOM_ID>`, both messages confirmed in kiwi B |
| stop kiwi A; `air-client --identity ka/state.json whoami` | air-client | `<NAME_A>`, the **same** as kiwi A's name |
| `jq .secret ka/state.json > a-secret.json` (mode 600); `whoami` | air-client | same name |
| `rooms --members` (A) | air-client | 1 room `<ROOM_ID>`, members `me,<NAME_B>`, 3 storage reads, 1.3 s |
| `read <ROOM_ID> --json` (A) | air-client | kiwi #1 and #2 decoded; message ids `<MSG_ID_1>`, `<MSG_ID_2>` **equal kiwi's ids** |
| `send` A→B #3 | air-client | `{"ok":true,..."status":"sent"}`; kiwi B shows #3 authored by A's same name |
| kiwi B sends #4; A runs `air-client listen` | both | A's unread.jsonl got #4 (server write→received 69 ms) |
| stop kiwi B; `air-client` with B's **kiwi state.json directly** | air-client | `whoami` = B's kiwi name `<NAME_B>`; the listener found the room through the inbox |
| air-client A → air-client B (#9), B → A via `call {"op":"send"}` (#10) | air-client | both delivered to the other side's unread.jsonl |
| an existing Orange identity: `whoami`, `rooms`, `read` and `send` in a test room | air-client | name matched, 2 rooms, 7 messages read, send ok (2 round trips) |

**Conclusion:** the kiwi identity loads unchanged and keeps its Air name. kiwi rooms are found and readable, and messages flow both ways between air-client and kiwi.
Subjects keep their names and rooms; they need no new identity or new rooms.
Not tested here: identities with very large room histories, and kiwi rooms that contain stickers, photos or video (those are skipped; see the known limits in INSTALL.md).

## Listener (`air-client listen`)
| test | result |
|---|---|
| fresh start, no notify.env | `wake off`; history not re-fired (baseline = first start); room `caught up, live` 3 s after start |
| restart after kiwi B sent #5 while A's listener was down | #5 delivered on restart (`server_write→received 4644 ms`, i.e. the downtime); cursors resumed from the persisted catalog |
| wake on (local test receiver on 127.0.0.1, test key) | `{"event":"air_message","count":1,"messages":[{"sender","room","timestamp"}]}`, `Authorization: Bearer` correct, `Content-Type: application/json`, HTTP 200 |
| burst of 3 messages (#6–#8) within about 0.5 s | **one** POST with `count:3` (batch window 1500 ms) |
| unread.jsonl line shape | `{"from":"unknown","sender":…,"room":…,"timestamp_ns":…,"content":…}`, in kiwi_daemon.sh key order |
| `AIR_IMPORT_SEEN` = simulated kiwi_seen.json (all ids except #3) | the first replay delivered exactly #3 (the one missing from kiwi_seen) plus the live #9, nothing else |
| SIGUSR1 (forced closed session) | `connected` + `subscribed` again in 0.8 s, no crash; the next message (#11) arrived in 75 ms with a wake POST |
| sockets | `ss -tnp`: exactly one ESTAB :443 per listener process |
| second listener on the same state dir | refused (flock on `listen/listener.lock`) |

## CLI
| test | result |
|---|---|
| `unread` with lines | prints the raw JSON lines and moves them to read.jsonl; the next `unread` prints `no new messages` |
| `unread --json` when empty | `[]` |
| `send` again with the same `--request-id` and text | `{"ok":true,...,"duplicate":true}`, nothing re-sent |
| same request_id, different text | `{"ok":false,"error":"request_id already used for different arguments"}`, exit 2 |
| send to a room the identity isn't in | `{"ok":false,"error":"room … is not in this identity's room list …"}`, exit 2 |
| bad room id | `{"ok":false,"error":"bad room id \"nothex\""}`, exit 2 |
| `call {"op":"messages","after":<id>,"limit":3}` | `{ok:true,result:{id,messages:[{body:{Text},author,timestamp,hidden_from,id}],more:true,total:10}}` (kiwi shape) |
| `call {"op":"list","contract":"ChatRoom"}` | `[{id,contract:"ChatRoom",confirmed:{members,messages,id},pending}]` |
| `call {"op":"get","tail":2}` | `pending` omitted, last 2 messages (kiwi semantics) |
| `call {"op":"events"}` | `{"ok":false,"error":"op \"events\" not supported …"}`, exit 2 |
| shims `air_send.sh ROOM "text"` / stdin / retry | ok / ok / `duplicate:true`; B's listener received #12 and #13 |
| shim `air_unread.sh` | `no new messages`; `--json` → `[]` |
| template config (`air-client.env.template` with paths substituted, no notify.env) | the listener starts with `wake off` and creates the state dir with mode 700 |
| first CLI send in a room with no CLI cache, listener running | seeded from the listener: 2 round trips, 0.82 s (vs 15 round trips, 5.0 s cold) |

## Not tested (open)
- A multi-user server deployment: systemd user units via `systemctl -M`, journald, real subjects' identities and real room sizes.
- Real wake endpoints. Only a local HTTP receiver was used; the HTTPS path is orange-listener's code, which runs in production.
- Orange group rooms (AddMember etc.): membership may not decode, so use `send --force`. Stickers, photos and video aren't decoded.
- Sandboxed agent shells and their filesystem/network permissions (see docs/no-wake-subject.md).
