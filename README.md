# BashKitten

BashKitten is a small, standalone Rust coding-agent harness for Ubuntu and
Debian. It has seven built-in tools (`bash`, `read`, `edit`, `write`, `grep`,
`find`, and `ls`), three provider modes, one process per active session, an
authenticated local Web UI, and a small GTK lifecycle/settings application.
Node.js and npm are not build or runtime dependencies.

The compatibility target is Pi post-`v0.85.0` (including GPT-6 Astra) at commit
`9841914c71a74d81abe07f751aefd271fd924e63`; see [PI_UPSTREAM.md](PI_UPSTREAM.md)
and [AGENTS.md](AGENTS.md) for the precise scope and intentional differences.

## Build the amd64 Debian package

Podman is the only host build dependency. Rust, Cargo, GTK headers, tests, and
packaging run inside the build container. Caches and artifacts default to the
data drive:

```sh
./scripts/build-deb-podman.sh
```

The package and SHA-256 file are written to:

```text
/run/media/user/Data/bashkitten-builds/artifacts/
```

Override that location with `BASHKITTEN_BUILD_ROOT` when needed.

## Run

```sh
systemctl --user start bashkitten.target
xdg-open http://127.0.0.1:3939
```

The first visit creates the one local Web UI account. Provider credentials,
Web login state, and skills live under `~/.config/bashkitten/`; session folders
live under `~/.local/share/bashkitten/sessions/`. Those directories and their
sensitive files are restricted to the current user.

Connect your OpenAI subscription in **Settings → Subscriptions** using
browser login or a device code. No Pi/Codex credential import is needed or
supported. Logout removes the BashKitten subscription credential (not another
application's login). OAuth secrets stay in Rust and the private credential file.
The folder control in the chat header changes the session's primary working
directory after any current turn settles; the sidebar automatically regroups it.

Settings has **App**, **Subscriptions**, **APIs**, and **llama.cpp** tabs. The
same folder picker is used for a chat's working folder, the App default working
folder, the llama.cpp models directory, and additional model folders. It supports
parent navigation, an absolute path, and creating a subfolder.

Configure compatible providers under **APIs**, using either HTTP or HTTPS
(including local servers such as `http://127.0.0.1:8000/v1`). Provider requests
connect directly; BashKitten does not use an HTTP proxy. Model downloads, cached
GGUF discovery, extra model folders, and GPU visibility controls are under
**llama.cpp**. Opening that tab does not start a Hugging Face search or download.

The composer **+** menu offers Attach files, Goal, and Compact context. Goal
adds a removable chip: send the ordinary message to make it the session goal.
`/goal OBJECTIVE` is also supported. Active goals continue normal turns until the
agent completes them with `bashkitten goal complete` through `bash`; Pause stops
automatic continuation. `/goal pause`, `/goal resume`, and `/goal clear` manage it.
`/compact` uses Pi's normal compaction path. The chat streams the summary while
compacting, restores its partial text after reload, and retains the finished
summary in an expandable scroll pane. The header shows the compaction count.

Secondary-click a sidebar chat for Rename chat and Delete chat. Secondary-click
a project folder for Delete project chats; this includes every chat recorded
under that exact folder. Shift+F10 opens the same menus. Deletion asks
for confirmation and removes only the selected chat's session directory,
including history and copied attachments. Its working folder and project files
are kept. A running worker is stopped before its storage is removed.

The llama.cpp settings list devices reported by the installed server with the
chosen GPU visibility environment. Native fit controls, fit memory margin,
minimum context, and maximum new tokens map directly to supported server flags.
The Hugging Face downloader is embedded Rust code adapted from SimpleHF. Select
repository files and a destination; files keep their original names and nested
paths. Downloads support pause, resume, cancellation and an optional saved HF
token. The default destination is `~/.local/share/bashkitten/models`.

Useful CLI commands:

```sh
bashkitten models --json
bashkitten auth status
bashkitten session start --prompt "Inspect this repository"
bashkitten session list
bashkitten send SESSION_ID --steer "Check the parser first"
```

BashKitten performs no analytics, crash reporting, update checks, remote asset
loading, or hidden model-catalog calls. Its application network traffic is
limited to configured model endpoints, OpenAI subscription OAuth/inference,
explicit Hugging Face actions, and loopback communication. Commands the agent
deliberately runs through `bash` remain ordinary processes with their normal
network access.

Licensed under Apache-2.0.
