# quick-bitwarden

A quick way to copy your [Bitwarden](https://bitwarden.com/) passwords, with a
fast [r-modals](https://github.com/Kevin-Ramirez127/r-modals) picker.

The picker and every dialog are rendered by **r-modals**
(borderless, searchable, X11 + Wayland), and the vault cache lives in a small
background daemon.

## How it works

`quick-bitwarden` is a **background daemon**:

It uses Bitwarden CLI `bw` to get your passwords, and shows them in an r-modals
window so you can select one and just copy it.

quick-bitwarden starts the daemon and stores the item list in memory so toggling
the window is instant. Passwords are read from `bw` only when you pick an item,
so a vault locked outside q-bw (for example with `bw lock`) is enforced
immediately.

Every time the window is hidden, it refreshes the cache only if more than
`QBW_REFRESH_INTERVAL` seconds elapsed (30 by default).

On **Enter** it fetches the password from `bw`, copies it to the clipboard and
in the background **deletes that password** from `cliphist` immediately, so the
clipboard history never keeps it; the password stays on the current clipboard
for `QBW_CLEAR_AFTER` seconds and then is cleared.

The daemon itself never opens a window: each dialog runs in a short-lived child
process, so `hide`/`toggle` can close it from the outside.

Only one daemon and one picker window run at a time. Concurrent launches are
serialised with a lock, and a `show` request that races an already-opening
picker is dropped instead of stacking a second window.

## Requirements

- **bitwarden-cli** (`bw`). See how to install bitwarden-cli on the
  [bitwarden-cli official documentation](https://bitwarden.com/help/cli/).
  You must have logged in at least once with `bw login`.
- A graphical session: **X11 or Wayland** (r-modals uses `eframe`/`egui`).
- A clipboard tool: **wl-copy**, **xclip** or **xsel**.
- Optional: **cliphist** (to keep passwords out of the clipboard history).
- Optional: **notify-send** for the "Copied" notification.

## Install

From source, with a Rust toolchain:

```sh
git clone https://github.com/Kevin-Ramirez127/quick-bitwarden
cd quick-bitwarden
cargo build --release
install -m755 target/release/quick-bitwarden ~/.local/bin/
```

Or:

```sh
cargo install --path .
```

## Usage

```sh
quick-bitwarden          # toggle the window (show/hide)
quick-bitwarden show     # open the search window
quick-bitwarden hide     # hide it (the daemon keeps running)
quick-bitwarden toggle   # explains itself
quick-bitwarden sync     # run `bw sync` and refresh the daemon cache
quick-bitwarden lock     # lock the vault and forget the cached session
quick-bitwarden hide-items  # hide the item list until a 3+ character search
quick-bitwarden show-items  # show the full item list again
quick-bitwarden stop     # stop the daemon and forget the cached session
quick-bitwarden restart  # restart the daemon (starts it if not running)
quick-bitwarden status   # daemon and vault status
quick-bitwarden status --toggle  # whether the picker is shown or hidden
quick-bitwarden --daemon # internal daemon entry point (for system startup)
quick-bitwarden --version / -v  # q-bw version
quick-bitwarden --help / -h     # list q-bw commands
```

### Search window

- Type the item name (or pick it from the list). Search is case-insensitive.
- **Enter** copies the password and closes the window.
- **Shift+Enter** copies the username instead of the password.
- **Escape** just closes the window.
- You can also type a name that is not listed and press Enter: it is forwarded
  to `bw get password`.

#### Prefixes

Queries that start with `>` are commands, not item names (defined in
`$QBW_BASE/config.toml`, see below). Type the prefix and press **Enter**:

- `>sync` — run `bw sync` and refresh the daemon cache.
- `>lock` — lock the vault and forget the cached session.
- `>hide items` — hide the item list until you search for 3+ characters.
- `>show items` — show the full item list again.
- `>restart` — restart the daemon.
- `>stop` — stop the daemon and forget the cached session.

#### Shortcuts

Key bindings live in the same `$QBW_BASE/config.toml`, materialised by the
daemon from a built-in template. `{exe}` in an action is replaced with the
running `quick-bitwarden` executable when the file is created (older configs
that still contain `{qbw}` are upgraded in place). By default:

- **Shift+Enter** picks the highlighted item but copies its **username**
  instead of the password.

One file holds both kinds of entries: `[[prefix]]` tables (above) and
`[[shortcut]]` tables. A shortcut can pick the highlighted row with a tag
(`pick = "username"`), or use any
[r-modals](https://github.com/Kevin-Ramirez127/r-modals) target (`option`,
`command`, `shell` + `args`) to add your own bindings. Edit the file and the
next window picks the change up; a malformed file is reported by the picker
instead of being overwritten.

`config.toml` replaces the separate `prefixes.toml`; a leftover `prefixes.toml`
(or `shortcuts.toml`) is migrated into it on the next daemon start.

#### Hiding the item list

`>hide items` (or `quick-bitwarden hide-items`) keeps the vault out of the
default view: the picker starts empty and only reveals items when the search
is meaningful.

- 3 or more characters: normal case-insensitive matching.
- Fewer than 3 characters: only an item whose name is typed **exactly** (or
  its short id) is shown.
- The choice is stored in `$XDG_STATE_HOME/quick-bitwarden/hide_items`
  (falling back to `~/.local/state/quick-bitwarden/hide_items`), so it
  survives `stop`/`restart` and a logout/reboot (and the daemon does not need
  to be restarted to pick it up). A marker left by an older version in
  `$QBW_BASE/hide_items` is migrated automatically. `>show items` removes it.

This is a shoulder-surfing aid, not an access-control boundary: the labels are
still sent to the picker process (which runs as you), and typing an item's name
exactly reveals it. It does not change what `bw` will return, and it does not
protect against someone who can already run commands as your user.

On tiling compositors (sway/Hyprland), float the window via its app id:

```
for_window [app_id="quick-bitwarden"] floating enable
```

### Sync

The program uses `bw`'s local cache. To pull the latest changes from your
account run one of:

- `quick-bitwarden sync`, or
- the `>sync` prefix inside the search window.

Both run `bw sync` and refresh the in-memory cache, so the new items are
available immediately. A notification tells you when the sync starts and
another when it ends. The cache also refreshes on its own once
`QBW_REFRESH_INTERVAL` has elapsed since the last update.

### Lock

To lock the vault again and drop the session token and cached items:

- `quick-bitwarden lock`, or
- the `>lock` prefix inside the search window.

The next `show` asks for the master password again. The daemon keeps running.

## Environment variables

| Variable | Default | Description |
| --- | --- | --- |
| `QBW_BASE` | `$XDG_RUNTIME_DIR/qbw` (fallback `/tmp/qbw`) | Daemon working directory (session, IPC, pid). It must be a private (`0700`) directory owned by you; q-bw refuses to use one that is not. |
| `QBW_CLEAR_AFTER` | `20` | Seconds the password stays on the clipboard |
| `QBW_REFRESH_INTERVAL` | `30` | Seconds between vault cache refreshes |

## Security

- The master password is never stored. The session token lives in a `0600`
  file inside the private runtime directory (`$QBW_BASE/s`) and is removed by
  `lock` and `stop`; `restart` keeps it so the daemon comes back unlocked.
- Core dumps are disabled at startup (`RLIMIT_CORE=0`, inherited by `bw` and
  the clipboard tools) and q-bw processes are not dumpable/ptraceable by other
  same-user processes (`PR_SET_DUMPABLE=0`).
- Vault passwords are never cached by the daemon or written to disk: each pick
  asks `bw` for the selected field, so locking the vault elsewhere stops
  further copies immediately.
- On copy: the password is removed from `cliphist` right away (with retries in
  case the watcher writes late) and the current clipboard is cleared after
  `QBW_CLEAR_AFTER` seconds (20 sec by default) by a detached helper, so
  `stop`/`restart` or a daemon crash does not leave it on the clipboard.

## Troubleshooting

- **"Not logged in"**: run `bw login` first.
- **The window doesn't appear**: make sure `bw` is on the daemon's `PATH` and
  that the daemon runs inside your graphical session (so r-modals can reach the
  display).
- **cliphist is not cleared**: check `cliphist store` with
  `cliphist list | grep <password>`.

## Features

**General**
- [x] Starts with system
- [x] Remove from history clipboard
- [x] Remove from user's clipboard in 20 seconds by default
- [x] Sync the vault from the picker (`>sync`) or with `quick-bitwarden sync`
- [x] Lock the vault from the picker (`>lock`) or with `quick-bitwarden lock`
- [x] Copy the username instead of the password with Shift+Enter
- [x] Install script

**Flags**
- [ ] Preserve the clipboard
- [ ] Change clean clipboard time
- [ ] Don't remove from history clipboard
- [ ] No notifications

## License

This project is licensed under the GNU General Public License v3.0 - see the
[LICENSE](LICENSE) file for details.
