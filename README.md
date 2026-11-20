# Rooste SSH

A fast, memory-safe SSH client for Linux, Windows and macOS, written in Rust: tabs and split panes, a
session manager, an SFTP file panel, port forwarding, and built-in key tools and agent.

*Rooste* is Estonian for *rust*, the language it's written in.

![Rooste SSH with two sessions, one above the other, an SFTP panel and the session manager](docs/images/split.png)

## Features

- **Sessions:** tabs, split panes and a searchable session manager with folders and tags.
  Duplicate a tab, reconnect automatically when a connection drops, and type into several sessions
  at once (broadcast input). Every action is also in the command palette.
- **Logins:** SSH agent (OpenSSH's, Pageant on Windows, or the built-in one), key files in
  OpenSSH or `.ppk` format, passwords saved in the system keyring, and keyboard-interactive login
  with one-time codes. Unknown host keys are confirmed and saved to `~/.ssh/known_hosts`.
- **Forwarding:** local, remote and dynamic (SOCKS) port forwarding with saved tunnels, jump
  hosts (`ProxyJump`), and agent and X11 forwarding.
- **tmux:** with `tmux -CC`, the server's tmux windows and panes show up as tabs and split panes,
  and keep running on the server when you disconnect.
- **Files:** an SFTP panel next to the terminal. Browse, download or upload files and whole
  folders, drop files onto it to upload, or choose a folder with "Download to…".
- **Terminal:** true color, Unicode, mouse support, search through the scrollback, per-tab zoom,
  logging sessions to text files and recording them for playback (asciinema `.cast`).
- **Scripts:** automate logins and command sequences with small [Rhai](https://rhai.rs) scripts
  that send text, wait for output and ask you questions; run them from a tab or on connect. See
  [docs/scripting.md](docs/scripting.md).
- **Tools:** generate keys, convert between `.ppk` and OpenSSH keys, and keep keys in a built-in
  SSH agent that other programs can use.
- **Also:** telnet, raw TCP and serial connections; light and dark themes; a portable mode; an
  opt-in per session for old devices that need outdated algorithms.
- **Works with what you have:** imports hosts from `~/.ssh/config` and saved sessions from PuTTY.

## Screenshots

<table>
  <tr>
    <td width="50%">
      <img src="docs/images/files.png" alt="The SFTP file panel next to a terminal">
      <p><b>SFTP file panel</b>: browse the server, download or upload files and folders.</p>
    </td>
    <td width="50%">
      <img src="docs/images/editor.png" alt="The session editor with a jump host and two tunnels">
      <p><b>Saved sessions</b>: jump hosts, tags and tunnels that start with the session.</p>
    </td>
  </tr>
  <tr>
    <td>
      <img src="docs/images/search.png" alt="Searching the terminal output">
      <p><b>Search</b> through everything a session printed (Ctrl+Shift+F).</p>
    </td>
    <td>
      <img src="docs/images/palette.png" alt="The command palette filtered to split commands">
      <p><b>Command palette</b>: every action a few keystrokes away (Ctrl+Shift+P).</p>
    </td>
  </tr>
  <tr>
    <td>
      <img src="docs/images/keygen.png" alt="The key generator">
      <p><b>Key tools</b>: generate keys, convert <code>.ppk</code> keys, run a built-in agent.</p>
    </td>
    <td>
      <img src="docs/images/light.png" alt="Rooste SSH in the light theme">
      <p><b>Light theme</b>, or dark, in Settings.</p>
    </td>
  </tr>
</table>

## Installing

Download a release from the [releases page](https://github.com/rooste-ssh/rooste-ssh/releases):

| Platform | Download | Notes |
|---|---|---|
| Windows | `rooste-ssh-*-x64.msi` | Installer, with Start menu entry |
| Windows (portable) | `rooste-ssh-*-windows-x64-portable.zip` | Unzip and run; settings stay in the folder |
| Debian, Ubuntu | `rooste-ssh-*-amd64.deb` | `sudo apt install ./rooste-ssh-*-amd64.deb` |
| Fedora, openSUSE | `rooste-ssh-*.x86_64.rpm` | `sudo dnf install ./rooste-ssh-*.x86_64.rpm` |
| Any Linux | `rooste-ssh.flatpak` | `flatpak install --user rooste-ssh.flatpak` |
| Any Linux | `rooste-ssh-*-x86_64.AppImage` | Make it executable and run it |
| Any Linux | `rooste-ssh-*-linux-x86_64.tar.gz` | Plain binaries |
| macOS, Apple Silicon (experimental) | `rooste-ssh-*-macos-arm64.dmg` | Drag *Rooste SSH* to Applications; see below |
| macOS, Intel (experimental) | `rooste-ssh-*-macos-x64.dmg` | Drag *Rooste SSH* to Applications; see below |

**macOS builds are new and not yet notarized by Apple.** The first time you open Rooste SSH, macOS
says it can't verify the developer: open **System Settings → Privacy & Security** and choose
**Open Anyway** (on older macOS, right-click the app and choose **Open**). Please report anything
that doesn't work on a Mac.

With Rust installed, you can also install from [crates.io](https://crates.io/crates/rooste-ssh):
`cargo install rooste-ssh` (the desktop app) and `cargo install rooste-cli`. `cargo binstall
rooste-ssh` downloads the release binaries instead of compiling.

Every release lists SHA-256 checksums in `SHA256SUMS`. Both programs are included: `rooste-ssh` (the
desktop app) and `rooste-cli` (command line).

## Using it

Start **Rooste SSH**, then either fill in the quick-connect form or add a session with **➕**.
Double-click a saved session to connect. Right-click a session or a tab for more actions.

| Shortcut | Action |
|---|---|
| Ctrl+Shift+T | Quick connect |
| Ctrl+Shift+P | Command palette |
| Ctrl+Shift+F | Find in the terminal output |
| Ctrl+Shift+W | Close the tab |
| Ctrl + / Ctrl − / Ctrl 0 | Zoom in, out, reset (Ctrl + mouse wheel works too) |
| Ctrl+Shift+C, Ctrl+Shift+V | Copy, paste (selecting text also copies it) |

On macOS, use ⌘ instead of Ctrl; ⌘C and ⌘V copy and paste.

### X11 forwarding

Turn on **Forward X11** in a session to show the server's graphical programs on your desktop.
Linux desktops work as they are. On Windows, run an X server such as VcXsrv or X410 first; on
macOS, XQuartz. The
server only ever receives a random cookie, never your display's real one.

### tmux

Run `tmux -CC` on the server (or `tmux -CC attach` to get back to a running session), or turn on
**Attach to tmux (-CC) on connect** in a session. Each tmux window opens as a tab and each pane as
a split. The tmux tab, marked in orange, opens new windows, runs tmux commands and detaches;
everything keeps running on the server until you attach again.

### Command line

```sh
rooste-cli user@host                              # interactive shell
rooste-cli user@host ls                           # run a command
rooste-cli -J bastion -L 5432:db:5432 -N user@host   # jump host + port forward
```

`rooste-cli` tries the SSH agent, then `~/.ssh/id_ed25519`, `id_ecdsa` and `id_rsa` (or keys given
with `-i`), then answers the server's questions (a password, one-time codes). Run
`rooste-cli --help` for all options.

## Configuration

Settings and saved sessions are stored in `config.toml` in the platform config directory
(`~/.config/rooste-ssh/` on Linux, `%APPDATA%\rooste-ssh\` on Windows,
`~/Library/Application Support/rooste-ssh/` on macOS). To run portably,
for example from a USB stick, put an empty file named `portable` next to the executable; the
configuration is then kept in `rooste-ssh.toml` beside it. Saved passwords go to the system keyring,
never into the configuration file.

## Building from source

Requires Rust 1.95 or newer. On Linux the desktop app needs the usual X11 or Wayland libraries
(present on any desktop).

```sh
cargo build --release
cargo run -p rooste-ssh              # desktop app (binary: rooste)
cargo run -p rooste-cli -- user@host
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for running the tests and the repository layout, and
[docs/ROADMAP.md](docs/ROADMAP.md) for the design and what is planned.

## Code signing policy

Windows releases are to be signed with free code signing provided by
[SignPath.io](https://signpath.io), certificate by
[SignPath Foundation](https://signpath.org). Until that is in place, releases are unsigned; check
downloads against `SHA256SUMS`.

- Committers and reviewers: [members of the rooste-ssh organization](https://github.com/orgs/rooste-ssh/people)
- Approvers: Martin Vooremäe ([@matbcvo](https://github.com/matbcvo))

Only binaries built from this repository by its GitHub Actions release workflow are signed, and
every release is approved by hand.

## Privacy

Rooste SSH collects no data and has no telemetry. It connects only to the servers you open
sessions to, and, if "Check for updates at startup" is on (Settings), asks GitHub's API
(`api.github.com`) once per start whether a newer release exists. That request sends nothing but
the usual HTTP headers, including the app version in the user agent. Saved sessions, settings and
session logs stay on your computer; saved passwords are kept in the system keyring.

## Security

Please report vulnerabilities privately, as described in [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE)
