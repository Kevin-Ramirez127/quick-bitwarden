# quick-bitwarden

A quick way to copy your [Bitwarden](https://bitwarden.com/) passwords that works on rofi/wofi window.


## How it works

  

`quick-bitwarden` is a **background daemon**:

It uses Bitwarden CLI   `bw` to get your passwords, and show it in a window to select and just copy it! 

quick-bitwarden starts the daemon and store the data on memory so toggling the window is instant.

Every time the window is hidden, it refreshes the cache only if more than `QBW_REFRESH_INTERVAL` seconds elapsed (30 by default).

On **Enter** it copies the password to the clipboard (from memory, no wait) and in the background **deletes that password** from `cliphist` immediately, so the clipboard history never keeps it; the password stays on the current clipboard for `QBW_CLEAR_AFTER` seconds and then is cleared.

  

## Requirements

-  **bitwarden-cli** (`bw`)` See how to install bitwarden-cli on [bitwarden-cli official documentation](https://bitwarden.com/help/cli/).

You must have logged in at least once with `bw login`.

-  **jq** to parse the CLI response.

- A launcher: **wofi** (Wayland) or **rofi** (X11/Wayland).

- A clipboard tool: **wl-copy**, **xclip** or **xsel**.

- Optional: **cliphist** (to keep passwords out of the clipboard history),

- **notify-send** / **zenity** / **rofi** for notifications.

  

## Install

Clone this repository `git clone https://github.com/Kevin-Ramirez127/quick-bitwarden`  

```sh

chmod  +x  quick-bitwarden
mv ./quick-bitwarden ~/.local/bin

```

  

## Usage

  

```sh

quick-bitwarden  # toggle the window (show/hide)

quick-bitwarden  show  # open the search window

quick-bitwarden  hide  # hide it (the daemon keeps running)

quick-bitwarden  toggle  # explains itself

quick-bitwarden  stop  # stop the daemon (keeps the session in /tmp/qbw/s)

quick-bitwarden  status  # daemon and vault status

quick-bitwarden  --daemon  # internal daemon entry point (you can use this for starts with your system)

quick-bitwarden  --version / -v  # q-bw version

quick-bitwarden  --help / -h  # list q-bw commands

```

  

### Search window

  

- Type the item name (or pick it from the list). Search is case-insensitive.

-  **Enter** copies the password and closes the window.

-  **Escape** just closes the window.

  

### Sync

  

The script uses `bw`'s local cache; to see changes from your account run `bw sync` (or add it wherever you prefer). The app cache refreshes once `QBW_REFRESH_INTERVAL` has elapsed since the last update.

  

## Environment variables

  

| Variable | Default | Description |


| `QBW_BASE` | `/tmp/qbw` | Daemon working directory (session, IPC, pid) |

| `QBW_CLEAR_AFTER` | `20` | Seconds the password stays on the clipboard |

| `QBW_REFRESH_INTERVAL` | `30` | Seconds between vault cache refreshes |

  

## Security

  

- The master password is never stored; the session token lives in `$QBW_BASE/s` and expires when the machine shuts down.

- Vault passwords live only in the daemon's memory, never written to disk.

- On copy: the password is removed from `cliphist` right away (with retries in case the watcher writes late) and the current clipboard is cleared after `QBW_CLEAR_AFTER` seconds (20 sec by default).

  

## Troubleshooting

  

-  **"Not logged in"**: run `bw login` first.

-  **The window doesn't appear**: make sure `bw` is on the daemon's `PATH` and

that you have a launcher (`rofi`/`wofi`).

-  **cliphist is not cleared**: check `cliphist store` with

`cliphist list | grep <password>`.

## Features
**General**
- [x] Starts with system
- [x] Remove from history clipboard
- [x] Remove from user's clipboard in 20 seconds by default
- [ ] Install script

**Flags**
- [ ] Preserve en cliboard
- [ ] Change clean clipboard time
- [ ] Don't remove from history clipboard
- [ ] No notifications
  

## License

This project is licensed under the GNU General Public License v3.0 - see the [LICENSE](LICENSE) file for details.