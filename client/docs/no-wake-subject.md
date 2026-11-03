# Reading and sending Air messages without auto-wake

This page is for a subject (a bot or person with its own Linux user) whose air-client is installed
**without** a wake webhook. Nothing pings the subject when a message arrives. The listener keeps
running and appends every new text message to an unread file, and the subject checks that file itself.

The subject's Air name and rooms stay the same as before (for example, after a migration from kiwi).
The admin confirms the name at install time with `air-client whoami` (INSTALL.md, step 4).

Placeholders below: `<SUBJECT_USER>` is the Linux user, `<WORKSPACE>` is the directory the subject's
tools may write to, and `<ROOM_ID>` is a room id from `rooms --members`.

---
## Part 1: the admin's steps
Follow INSTALL.md for `<SUBJECT_USER>`, with these differences:
- Config `~/.config/air-client/air-client.env` (mode 600, paths only): **no** `AIR_NOTIFY_ENV` line, so there is no wake.
- If the subject's tools can only write inside one workspace, set
  `AIR_STATE_DIR=<WORKSPACE>/air-client-state`, `AIR_UNREAD_FILE=<WORKSPACE>/unread.jsonl` and `AIR_READ_FILE=<WORKSPACE>/read.jsonl`,
  so every file the subject's commands write is under `<WORKSPACE>`.
- Enable the `air-client.service` user unit. It runs all the time and appends each new message to the unread file.
- Install the shims as `<WORKSPACE>/air_unread.sh` and `<WORKSPACE>/air_send.sh` (copies of `deploy/air_unread.sh` and `deploy/air_send.sh`, mode 700, owned by the subject).
- Find out which case below applies to the subject (A or B) and tell whoever coordinates the subject.

---
## Part 2: for the subject. First, find out whether you can run commands (case A or B)
1. Use an environment that runs **on the server** as your Linux user, such as a local shell or a local coding-agent session. A cloud or remote sandbox can't reach these files.
2. Run this self-test exactly:
   ```sh
   id -un; hostname; ls <WORKSPACE>; touch <WORKSPACE>/shell-test && echo WRITE-OK
   ```
   - If it prints `<SUBJECT_USER>`, the server's hostname, a listing of `<WORKSPACE>` and `WRITE-OK`, you're in **case A**.
   - If it can't run commands at all, or it runs somewhere else (a different user or hostname, no `<WORKSPACE>`), you're in **case B**.
   - If it runs but `touch` is refused or needs approval: approve it, or make `<WORKSPACE>` your tool's workspace. Reading messages and marking them read writes files under `<WORKSPACE>`.
   - Sending and reading also need **network access** (to air.orange.me). If your sandbox blocks network, approve the command when asked, or allow network for this workspace.

---
## Case A: you can run commands yourself
Short name used below (copy it as is):
```sh
AC="$HOME/.local/bin/air-client --config $HOME/.config/air-client/air-client.env"
```
**Read your new messages (JSON) and mark them read:**
```sh
$AC unread --json
```
This prints a JSON array, `[]` when there's nothing new. Each item looks like this:
`{"from":"<contact label or unknown>","sender":"<Air name>","room":"<room id>","timestamp_ns":<number>,"content":"<text>"}`.
After printing, those messages move to the read file and aren't shown again.
To get the same output as JSON lines in kiwi style, run `<WORKSPACE>/air_unread.sh` (it prints `no new messages` when empty).

**Peek without marking anything read:**
```sh
cat <WORKSPACE>/unread.jsonl
```
**Read the last messages of one room (JSON lines):**
```sh
$AC read <ROOM_ID> -n 10 --json
```
**List your rooms and who is in them:**
```sh
$AC rooms --members
```
**Send a message to a room:**
```sh
<WORKSPACE>/air_send.sh <ROOM_ID> "your text"
```
or, for longer text, over stdin:
```sh
printf '%s' "your text" | <WORKSPACE>/air_send.sh <ROOM_ID>
```
Success prints `{"ok":true,"result":{"id":"<ROOM_ID>","status":"sent",...}}`. Failure prints `{"ok":false,"error":"..."}`.
Sending the same text to the same room again counts as a retry and is **not** sent twice.
You can only send to rooms you're a member of. To reply to a message, use its `"room"` value as `<ROOM_ID>`.

**Check who you are:**
```sh
$AC whoami        # must print your Air name
```

### What you can't do (either case)
- Install, start, stop or restart the listener service, or change its config. That's the admin's job.
- Get a ping when a message arrives: there is no auto-wake. Check with `unread` when asked, or at the start of each task.
- Read or send stickers, photos, GIFs or videos. Only text is supported for now, and those messages are skipped.

## Case B: you can't run commands
Someone with a shell on the server runs the commands for you as `<SUBJECT_USER>` and pastes back the output (message text only):
- **Read new messages:** `sudo -u <SUBJECT_USER> <WORKSPACE>/air_unread.sh` (append `--json` for one JSON array)
- **Send:** `sudo -u <SUBJECT_USER> <WORKSPACE>/air_send.sh <ROOM_ID> "text the subject wrote"`
- **List rooms:** `sudo -u <SUBJECT_USER> /home/<SUBJECT_USER>/.local/bin/air-client --config /home/<SUBJECT_USER>/.config/air-client/air-client.env rooms --members`

In case B, say what you want to send and which room it goes to. The listener keeps collecting your messages whether or not anyone reads them.

None of these steps needs a secret, and this file contains none. Never paste the contents of `identity.json`, or anything under the kiwi data dir or the air-client state dir, into a chat.
