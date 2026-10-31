# AgentControl

**Mission control for your coding agents — they run on your server, you
drive them from anywhere.**

AgentControl is a lightweight desktop & mobile app that pilots AI coding
agents running **detached** on your own Linux server. Close the app:
nothing dies — the agents keep working. Reopen it on your laptop, your
phone, another machine: every session is there, full history, pending
questions, mid-task.

Run several agents in parallel, on one server or many. Organize them,
queue their work, let them delegate to each other — and supervise the
whole fleet from your desk or your couch.

## Screenshots

Markdown chat with expandable tool calls, session groups, prompt queue,
pull-request tracking — desktop:

![AgentControl desktop — agent chat](docs/screenshots/desktop-chat.png)

Browse the remote workspace: file tree, in-app editing, image and video
preview:

![AgentControl desktop — remote file explorer](docs/screenshots/desktop-files.png)

Same sessions, same agents — native mobile UI on Android:

| Chat | Sessions & groups | Files |
|:-:|:-:|:-:|
| ![Android chat](docs/screenshots/android-chat.png) | ![Android drawer](docs/screenshots/android-drawer.png) | ![Android files](docs/screenshots/android-files.png) |

## Install

### You need

- **A Linux server** (x86_64 or aarch64) reachable over SSH — that's
  where the agents run.
- **At least one coding agent** on that server. Anything that speaks
  [ACP](https://agentclientprotocol.com) over stdio works: Devin
  (`devin acp`) and Cursor (`cursor-agent acp`) natively, Codex and
  Claude through small adapters.
- **The app** on your machine — pick yours on the
  [**releases** page](https://github.com/Wameuh/AgentControl/releases):

  | Platform | Download |
  |---|---|
  | Windows | NSIS installer (`.exe`), `.msi`, or portable `.exe` |
  | macOS (Apple Silicon) | `.dmg` — on first launch, right-click the app → **Open** |
  | Ubuntu / Linux | `.deb`, AppImage, or the standalone binary |
  | Android | `.apk` (arm64) — allow installs from this source |

  Rather compile it yourself? See [build from source](#for-developers).

### First launch

1. Open the app → **New connection** → type `user@your-server`.
2. No daemon there? AgentControl offers to **install it for you** over
   SSH — binaries deployed and auto-started in seconds.
3. Hit **New agent**, choose the agent, a working directory, a model —
   send your first prompt. You can close the app now: the agent doesn't
   care, it keeps going.

SSH uses your existing keys and `~/.ssh/config`. No keys on that server?
The built-in SSH transport takes a password, or generates a dedicated
app key for you to authorize.

### Pairing a phone (or a second machine) by QR code

On a saved SSH connection, hit the **QR button** to pair a device in
seconds — on the other side, **Scan QR** in the connection form does
the rest:

- The desktop installs a **one-shot, short-lived key** on the server
  (a `restrict`-ed `authorized_keys` line bound to
  `~/.agentcontrol/pair.sh <nonce> <rport>`, ≤ 5 min, self-destructing
  on use) and shows a QR carrying that key plus the server's host-key
  fingerprints.
- The device scans, confirms `user@host`, authenticates with the
  one-shot key — the server's host key is **pinned by the scanned
  fingerprints** (a MITM can't slip in), and `pair.sh` installs the
  device's own key restricted to `127.0.0.1:<rport>` port forwarding.
- The server answers `pair-ok:<fingerprint>` — verified against the
  device key — and the one-shot line disappears.
- Afterwards, every connection from that profile keeps **pinning the
  server host keys**: a divergence (rotation, MITM attempt) is flagged
  and requires explicit reconfirmation.

Two fallbacks stay available under *Other methods* in the pairing
dialog: **endpoint-only** (scan the address, install the device's
public key yourself) and **permanent credentials** (the profile's own
key/password — opt-in only, with a warning, for servers where the
hardened pairing can't run).

Housekeeping on the server: `~/.agentcontrol/pair.sh --sweep` removes
expired or orphaned one-shot lines and stale state files (orphans are
kept 24 h). A user `crontab` entry every 15 min is installed
automatically at the first pairing; pairings also trigger a sweep.
`~/.agentcontrol/pair.log` records timestamps, nonces, statuses and
fingerprints — never secrets — and paired devices can be revoked from
the dialog's *Paired devices* list.

## What you can do

- **Run many agents at once**, on one server or several. Every session
  is detached: kill the app, reboot your machine — the agents don't
  notice. If the server-side daemon itself crashes, working sessions are
  automatically resumed.
- **Organize sessions into groups** — nested, drag & drop — filter them.
- **Drive several servers side by side**: one window per connection, each
  tinted its own color with the host in the title bar. Spawn another
  instance from a connection's context menu or the saved-connection
  picker — under the hood it's just `agentcontrol --conn <conn-key>`.
- **Queue messages** while an agent is busy, or **steer it mid-turn** to
  inject a correction right after the current step.
- **Let agents work together**: an agent can spawn sub-sessions,
  delegate tasks and exchange messages with its peers — you follow the
  whole conversation tree, and can fork a session into a second agent
  yourself.
- **Put sessions to sleep** to free CPU and RAM on the server; they wake
  up by themselves the next time you — or another agent — write to them,
  context intact.
- **Answer permissions and questions wherever you are**: approval
  prompts wait for you across disconnects.
- **Work the remote filesystem**: edit files in place (encoding-aware,
  conflict detection on save), preview images, stream video, read
  rendered markdown, download anything.
- **Open a remote terminal** next to the chat when you need a shell.
- **Track the pull requests** your agents opened — grouped by repo,
  status at a glance, linked back to the session that owns each one.
- **Search and replay everything**: the daemon journals every event, so
  history is never lost.
- **Tune the feel**: dark & light themes, accent colors, animated
  background, sound alerts, English or French UI.
- **Stay up to date effortlessly**: the desktop app updates itself in
  place, upgrades the daemon on your server over SSH, and an opt-in
  channel serves release candidates.

Prefer a browser to an app? The daemon can also **serve the same UI over
HTTP** on the server (`bind_http` in `config.toml` — loopback + SSH
tunnel, token-protected): zero install on the client side.

## For developers

AgentControl is a Rust workspace (daemon, protocol, client core, CLI)
plus a Tauri 2 front end (vanilla TypeScript).

- Architecture, protocol and design rationale: [ANALYSE.md](ANALYSE.md)
- Review registry across forges (GitHub/GitLab/Gerrit):
  [docs/forges.md](docs/forges.md)
- Manual server setup & systemd unit:
  [packaging/INSTALL.md](packaging/INSTALL.md)
- Repo workflow & conventions: [AGENTS.md](AGENTS.md)

Build from source:

```bash
cargo build --release                          # daemon + CLI
cd client && npm install && npx tauri build    # desktop app
```

## License

[MIT](LICENSE)
