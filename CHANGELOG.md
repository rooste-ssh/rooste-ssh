# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- A scrollbar in the terminal: drag it, or click to jump anywhere in the scrollback.

### Changed

- The mouse wheel scrolls three lines per notch instead of one.
- Dialogs highlight their main button in blue, and in red when it deletes or closes something.

- Updated dependencies, including egui 0.37. Some small icons changed so that they all come from
  the app's own fonts (`×`, `⏷`, `🔘`, `⏺`) instead of depending on the system's fonts.

### Security

- Updated `crypto-bigint`, `ctutils`, `rustls-webpki` and `webpki-root-certs` for
  RUSTSEC-2026-0387, RUSTSEC-2026-0385, RUSTSEC-2026-0382 and RUSTSEC-2026-0383.

### Fixed

- Exporting an ECDSA key to `.ppk` could write a file that couldn't be read back (about one
  P-256 key in several hundred, when its private number starts with a zero byte).

## [0.4.0] - 2026-10-17

### Changed

- The desktop app's command is now `rooste-ssh` (it was `rooste`), matching the project, package
  and crate names. On Windows the program is `rooste-ssh.exe`; if you use the portable zip, delete
  the old `rooste.exe` after unzipping the new version over it. The portable settings file is now
  `rooste-ssh.toml`; an existing `rooste.toml` is renamed automatically. The command-line client
  is still `rooste-cli`.

### Fixed

- A new tab or split now takes the keyboard focus, and the tab bar shows it right away. Before,
  the focus could stay in (or jump back to) the other half of a split.
- Switching tabs in one half of a split no longer flashes the focus line in the other half.

## [0.3.0] - 2026-10-16

### Added

- Scripting: Rhai scripts in the scripts folder can send text, wait for output, ask questions and
  show messages. Run them from a tab's right-click menu or the command palette, or automatically
  when a session connects ("Run on connect"). See `docs/scripting.md`.
- Session recordings: "Record session…" (Tab menu, the tab's right-click menu, the command palette)
  saves an asciinema `.cast` file that replays the session with its timing. Only output is
  recorded, never what you type.
- A snap package (`rooste-ssh`), to be published in the Snap Store once it's approved there.

### Changed

- The selected tab is easier to see: a line along its top (blue where your typing goes, grey in
  the other half of a split), and unselected tabs look like tabs. Clicking a tab moves the
  keyboard focus to it.

### Fixed

- A crash when a server sent output that left the cursor on the second half of a wide character
  in the first column.
- A crash importing a PuTTY session whose name has a non-ASCII character right after `%`.

## [0.2.1] - 2026-10-15

### Added

- Published on crates.io: `cargo install rooste-ssh` and `cargo install rooste-cli` (or
  `cargo binstall` for the release binaries). The app's package is now named `rooste-ssh`.
- Right-click menus in the session list (new session, quick connect, imports; a new session in
  a folder), and a centered Connect button on the welcome screen.

## [0.2.0] - 2026-10-14

### Added

- Experimental macOS builds (`.dmg` for Apple Silicon and Intel), not yet notarized. Shortcuts use
  ⌘ instead of Ctrl, ⌘C and ⌘V copy and paste, and settings are kept in
  `~/Library/Application Support/rooste-ssh`.

## [0.1.1] - 2026-10-14

### Changed

- The project moved to its own organization:
  [github.com/rooste-ssh/rooste-ssh](https://github.com/rooste-ssh/rooste-ssh). Old links
  redirect; the update check now asks the new address.
- The Linux app ID is now `ee.prica.rooste-ssh` (it was `io.github.matbcvo.rooste-ssh`) and no
  longer depends on where the repository lives. If you installed the 0.1.0 Flatpak, uninstall it
  and install this one; other packages and the Windows installer upgrade as usual. Settings,
  saved sessions and saved passwords are kept.

## [0.1.0] - 2026-10-14

The first release.

### Sessions and terminal

- SSH sessions in tabs and split panes, with a searchable session manager (folders, tags) and a
  command palette.
- Duplicate a tab, reconnect automatically when a connection drops, and broadcast input to
  several sessions.
- Telnet, raw TCP and serial connections.
- True color, Unicode and mouse support; search through the scrollback (Ctrl+Shift+F); per-tab
  zoom (Ctrl + / Ctrl − / Ctrl 0); logging sessions to plain-text files.
- Light and dark themes; portable mode.

### Logins and security

- SSH agent (OpenSSH, Pageant on Windows, or the built-in agent), key files in OpenSSH and
  `.ppk` format (including old encrypted PEM keys), passwords in the system keyring, and
  keyboard-interactive login with one-time codes.
- Host keys checked against `~/.ssh/known_hosts`; unknown keys are confirmed first.
- Outdated algorithms only when enabled for a session.

### Forwarding and files

- Local, remote and dynamic (SOCKS) port forwarding with saved tunnels; jump hosts
  (`ProxyJump`); agent and X11 forwarding.
- An SFTP file panel: browse, upload and download files and folders, drag and drop uploads,
  "Download to…".

### Tools

- Key generator, `.ppk` ⇄ OpenSSH key converter and a built-in SSH agent.
- Import from `~/.ssh/config` and from PuTTY saved sessions.
- `rooste-cli`, a command-line client.

### Distribution

- Windows MSI installer and portable zip; Linux `.deb`, `.rpm`, AppImage, `.tar.gz` and Flatpak.
- Optional check for new releases at startup.

[0.4.0]: https://github.com/rooste-ssh/rooste-ssh/releases/tag/v0.4.0
[0.3.0]: https://github.com/rooste-ssh/rooste-ssh/releases/tag/v0.3.0
[0.2.1]: https://github.com/rooste-ssh/rooste-ssh/releases/tag/v0.2.1
[0.2.0]: https://github.com/rooste-ssh/rooste-ssh/releases/tag/v0.2.0
[0.1.1]: https://github.com/rooste-ssh/rooste-ssh/releases/tag/v0.1.1
[0.1.0]: https://github.com/rooste-ssh/rooste-ssh/releases/tag/v0.1.0
