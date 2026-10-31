//! Renders the terminal widget to `target/screenshots/` for visual review.
//! Needs a wgpu adapter (a software Vulkan driver is fine), so it is not
//! part of the default test run:
//!
//! ```text
//! cargo test -p rooste-ssh --test screenshot -- --ignored
//! ```

use std::path::PathBuf;

use egui_kittest::Harness;
use rooste_ssh::terminal_view::TerminalView;
use rooste_terminal::Terminal;

fn demo() -> String {
    let mut s = String::new();
    s.push_str("\x1b[1;32mmartin@server\x1b[0m:\x1b[1;34m~/project\x1b[0m$ ls --color\r\n");
    s.push_str("\x1b[1;34msrc\x1b[0m  \x1b[1;34mtests\x1b[0m  Cargo.toml  \x1b[1;32mbuild.sh\x1b[0m  \x1b[31marchive.tar.gz\x1b[0m\r\n");
    s.push_str("\x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m \x1b[4munderline\x1b[0m \x1b[9mstrike\x1b[0m \x1b[7minverse\x1b[0m \x1b[2mdim\x1b[0m\r\n");
    for i in 0..16 {
        s.push_str(&format!("\x1b[48;5;{i}m  "));
    }
    s.push_str("\x1b[0m\r\n");
    for i in 0..48 {
        let v = i * 5;
        s.push_str(&format!("\x1b[48;2;{v};{};{}m ", 120 + i, 255 - v));
    }
    s.push_str("\x1b[0m\r\n");
    s.push_str("╔═══╗ ▁▂▃▄▅▆▇█ ░▒▓ ┏━━┓\r\n║ ║ ║ ▀▄▌▐       ┗━━┛\r\n╚═══╝\r\n");
    s.push_str("┌──────────┬────────┐\r\n│ Unicode  │ õäöü € │\r\n├──────────┼────────┤\r\n│ Wide     │ 日本語 │\r\n└──────────┴────────┘\r\n");
    s.push_str("\x1b[33mwarning:\x1b[0m selection, scrollback and \x1b[38;5;208m256 colors\x1b[0m work\r\n");
    s.push_str("\x1b[1;32mmartin@server\x1b[0m:\x1b[1;34m~/project\x1b[0m$ vim");
    s
}

#[test]
#[ignore = "renders with wgpu; run manually to inspect the output"]
fn render_terminal() {
    let mut terminal = Terminal::new(80, 24);
    terminal.advance(demo().as_bytes());
    let state = (terminal, TerminalView::default());
    let mut harness = Harness::builder().with_size([760.0, 330.0]).build_ui_state(
        |ui, (terminal, view): &mut (Terminal, TerminalView)| {
            view.show(ui, terminal);
        },
        state,
    );
    rooste_ssh::fonts::install(&harness.ctx);
    harness.run();
    // Re-feed after the widget sized the grid to the window.
    let (terminal, _) = harness.state_mut();
    terminal.advance(b"\x1b[2J\x1b[H");
    terminal.advance(demo().as_bytes());
    terminal.select_start(0, 7, false, rooste_terminal::SelectionKind::Simple);
    terminal.select_update(7, 7, true);
    harness.run();

    let image = harness.render().expect("render with wgpu");
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("terminal.png");
    image.save(&path).unwrap();
    println!("saved {}", path.display());
}

#[test]
#[ignore = "renders with wgpu; run manually to inspect the output"]
fn render_box_drawing() {
    let text = "╔═══╦═╗ ┏━┳━┓ ┌─┬─┐\r\n║ A ║B║ ┃x┃y┃ │1│2│\r\n╠═══╬═╣ ┣━╋━┫ ├─┼─┤\r\n╚═══╩═╝ ┗━┻━┛ └─┴─┘\r\n▁▂▃▄▅▆▇█ ▏▎▍▌▋▊▉ ░▒▓ ▀▄";
    let mut view = TerminalView::default();
    view.font_size = 28.0;
    let state = (Terminal::new(40, 6), view);
    let mut harness = Harness::builder().with_size([520.0, 220.0]).build_ui_state(
        |ui, (terminal, view): &mut (Terminal, TerminalView)| {
            view.show(ui, terminal);
        },
        state,
    );
    harness.run();
    harness.state_mut().0.advance(text.as_bytes());
    harness.state_mut().0.advance(b"\x1b[?25l");
    harness.run();
    let image = harness.render().expect("render with wgpu");
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots");
    std::fs::create_dir_all(&dir).unwrap();
    image.save(dir.join("boxes.png")).unwrap();
}

#[test]
#[ignore = "renders with wgpu; run manually to inspect the output"]
fn render_app() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use rooste_core::{Config, MemoryStore, Profile};
    use rooste_ssh::RoosteApp;
    use rooste_testkit::{TEST_PASSWORD, TEST_USER, TestServer};

    let net = tokio::runtime::Runtime::new().unwrap();
    let server = net.block_on(TestServer::start());
    let dir = tempfile::tempdir().unwrap();
    let known_hosts = dir.path().join("known_hosts");
    rooste_protocols::ssh::known_hosts::learn(
        &known_hosts,
        "127.0.0.1",
        server.addr.port(),
        &server.host_key,
    )
    .unwrap();
    let p = |name: &str, folder: &str| Profile {
        name: name.into(),
        folder: folder.into(),
        host: format!("{}.example.com", name.to_lowercase().replace(' ', "-")),
        user: "deploy".into(),
        ..Profile::default()
    };
    let live = Profile {
        name: "Test server".into(),
        host: "127.0.0.1".into(),
        port: Some(server.addr.port()),
        user: TEST_USER.into(),
        use_agent: false,
        save_password: true,
        ..Profile::default()
    };
    let config = Config {
        profiles: vec![
            p("Web 1", "Work/Production"),
            p("Web 2", "Work/Production"),
            p("Database", "Work/Production"),
            p("Staging", "Work"),
            p("Raspberry Pi", "Home"),
            live.clone(),
        ],
        ..Config::default()
    };
    let app = RoosteApp::with_config(config, None, Arc::new(MemoryStore::default()))
        .with_known_hosts(known_hosts);
    app.secrets().set(&live.id, TEST_PASSWORD).unwrap();
    let mut harness = Harness::builder()
        .with_size([1000.0, 560.0])
        .build_ui_state(|ui, app: &mut RoosteApp| app.show(ui), app);
    rooste_ssh::fonts::install(&harness.ctx);
    let ctx = harness.ctx.clone();
    harness.state_mut().connect_profile(&ctx, &live.id);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !harness
        .state()
        .tabs()
        .any(|t| t.terminal.screen_text().contains('$'))
    {
        assert!(Instant::now() < deadline);
        harness.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    harness.event(egui::Event::Text("hello from rooste".into()));
    harness.key_press(egui::Key::Enter);
    for _ in 0..20 {
        harness.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    harness.run();
    let image = harness.render().expect("render with wgpu");
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots");
    std::fs::create_dir_all(&out).unwrap();
    image.save(out.join("app.png")).unwrap();
}

/// The welcome screen and the main dialogs, one image each.
#[test]
#[ignore = "renders with wgpu; run manually to inspect the output"]
fn render_dialogs() {
    use std::sync::Arc;

    use egui_kittest::kittest::Queryable;
    use rooste_core::{Config, MemoryStore, Profile};
    use rooste_ssh::RoosteApp;

    let p = |name: &str, folder: &str| Profile {
        name: name.into(),
        folder: folder.into(),
        host: format!("{}.example.com", name.to_lowercase().replace(' ', "-")),
        user: "deploy".into(),
        ..Profile::default()
    };
    let config = Config {
        profiles: vec![
            p("Web 1", "Work/Production"),
            p("Database", "Work/Production"),
            p("Staging", "Work"),
            p("Raspberry Pi", "Home"),
        ],
        ..Config::default()
    };
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots");
    std::fs::create_dir_all(&out).unwrap();
    let shots: [(&str, &[&str]); 6] = [
        ("welcome", &[]),
        ("update-banner", &[]),
        ("welcome-light", &[]),
        ("profile-editor", &["Session", "New session…"]),
        ("settings", &["Session", "Settings…"]),
        ("key-generator", &["Tools", "Key generator…"]),
    ];
    for (name, clicks) in shots {
        let mut config = config.clone();
        if name.ends_with("-light") {
            config.settings.theme = rooste_core::Theme::Light;
        }
        let mut app = RoosteApp::with_config(config, None, Arc::new(MemoryStore::default()));
        if name == "update-banner" {
            app.update_banner = Some(rooste_ssh::update::Release {
                version: "0.2.0".into(),
                url: "https://example.com".into(),
                download: "https://example.com".into(),
            });
        }
        let mut harness = Harness::builder()
            .with_size([1000.0, 640.0])
            .build_ui_state(|ui, app: &mut RoosteApp| app.show(ui), app);
        rooste_ssh::fonts::install(&harness.ctx);
        harness.run();
        for label in clicks {
            harness.get_by_label(label).click();
            harness.run();
        }
        let image = harness.render().expect("render with wgpu");
        image.save(out.join(format!("{name}.png"))).unwrap();
    }
}

/// A tab whose login failed with a long error message.
#[test]
#[ignore = "renders with wgpu; run manually to inspect the output"]
fn render_failed_tab() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use egui_kittest::kittest::Queryable;
    use rooste_core::{Config, MemoryStore};
    use rooste_ssh::RoosteApp;
    use rooste_testkit::{TEST_USER, TestServer};

    let net = tokio::runtime::Runtime::new().unwrap();
    let server = net.block_on(TestServer::start());
    let dir = tempfile::tempdir().unwrap();
    let known_hosts = dir.path().join("known_hosts");
    rooste_protocols::ssh::known_hosts::learn(
        &known_hosts,
        "127.0.0.1",
        server.addr.port(),
        &server.host_key,
    )
    .unwrap();
    let not_a_key = dir
        .path()
        .join("a-folder-with-a-rather-long-name")
        .join("another-long-folder-name-to-make-this-wrap")
        .join("private_key3.openssh");
    std::fs::create_dir_all(not_a_key.parent().unwrap()).unwrap();
    std::fs::write(&not_a_key, "not a key\n").unwrap();

    let mut app = RoosteApp::with_config(Config::default(), None, Arc::new(MemoryStore::default()))
        .with_known_hosts(known_hosts);
    app.quick.host = "127.0.0.1".into();
    app.quick.port = server.addr.port().to_string();
    app.quick.user = TEST_USER.into();
    app.quick.use_agent = false;
    app.quick.key_file = not_a_key.display().to_string();
    let mut harness = Harness::builder()
        .with_size([1000.0, 640.0])
        .build_ui_state(|ui, app: &mut RoosteApp| app.show(ui), app);
    rooste_ssh::fonts::install(&harness.ctx);
    harness.run();
    harness.get_by_label("Connect").hover();
    harness.run();
    harness.get_by_label("Connect").click();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !harness.state().tabs().any(|t| t.has_ended()) {
        assert!(
            Instant::now() < deadline,
            "the login should fail: {:?}",
            (
                harness
                    .state()
                    .tabs()
                    .map(|t| t.status_text())
                    .collect::<Vec<_>>(),
                &harness.state().quick_error
            )
        );
        harness.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    for _ in 0..5 {
        harness.step();
    }
    let image = harness.render().expect("render with wgpu");
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots");
    std::fs::create_dir_all(&out).unwrap();
    image.save(out.join("failed-tab.png")).unwrap();
}

/// The SFTP file panel with a few entries, two of them selected.
#[test]
#[ignore = "renders with wgpu; run manually to inspect the output"]
fn render_files_panel() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use rooste_core::{Config, MemoryStore, Profile};
    use rooste_ssh::RoosteApp;
    use rooste_testkit::{TEST_PASSWORD, TEST_USER, TestServer};

    let net = tokio::runtime::Runtime::new().unwrap();
    let server = net.block_on(TestServer::start());
    let root = server.sftp_root().to_owned();
    std::fs::create_dir(root.join("logs")).unwrap();
    for name in ["backup.tar.gz", "notes.txt", "report.pdf"] {
        std::fs::write(root.join(name), name.repeat(100)).unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let known_hosts = dir.path().join("known_hosts");
    rooste_protocols::ssh::known_hosts::learn(
        &known_hosts,
        "127.0.0.1",
        server.addr.port(),
        &server.host_key,
    )
    .unwrap();
    let live = Profile {
        name: "Test server".into(),
        host: "127.0.0.1".into(),
        port: Some(server.addr.port()),
        user: TEST_USER.into(),
        use_agent: false,
        save_password: true,
        ..Profile::default()
    };
    let config = Config {
        profiles: vec![live.clone()],
        ..Config::default()
    };
    let app = RoosteApp::with_config(config, None, Arc::new(MemoryStore::default()))
        .with_known_hosts(known_hosts);
    app.secrets().set(&live.id, TEST_PASSWORD).unwrap();
    let mut harness = Harness::builder()
        .with_size([1000.0, 560.0])
        .build_ui_state(|ui, app: &mut RoosteApp| app.show(ui), app);
    rooste_ssh::fonts::install(&harness.ctx);
    let ctx = harness.ctx.clone();
    harness.state_mut().connect_profile(&ctx, &live.id);
    let deadline = Instant::now() + Duration::from_secs(10);
    let listed = |app: &RoosteApp| {
        app.tabs()
            .next()
            .and_then(|t| t.files.as_ref())
            .is_some_and(|f| f.entries.len() == 4)
    };
    while !listed(harness.state()) {
        assert!(Instant::now() < deadline);
        if harness
            .state()
            .tabs()
            .any(|t| t.files.is_none() && t.state == rooste_ssh::app::TabState::Connected)
        {
            harness
                .state_mut()
                .tabs_mut()
                .next()
                .unwrap()
                .toggle_files();
        }
        harness.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    let files = harness
        .state_mut()
        .tabs_mut()
        .next()
        .unwrap()
        .files
        .as_mut()
        .unwrap();
    files.selected = files.entries[1..3].iter().map(|e| e.path.clone()).collect();
    for _ in 0..5 {
        harness.step();
    }
    let image = harness.render().expect("render with wgpu");
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots");
    std::fs::create_dir_all(&out).unwrap();
    image.save(out.join("files-panel.png")).unwrap();
}

/// Screenshots for the README, written to `target/screenshots/readme-*.png`:
/// two sessions one above the other, the file panel, the session editor,
/// search, the command palette, the key generator, and the light theme.
///
/// The sessions connect to real servers and run real commands, so all
/// terminal output and files are genuine. Start three containers from
/// `crates/rooste-testkit/docker/screenshot` with the hostnames `web-1`,
/// `db-primary` and `staging`, then set `ROOSTE_SCREENSHOT_SERVERS` to their
/// SSH ports (`web-1=2241,db-primary=2242,staging=2243`) and
/// `ROOSTE_SCREENSHOT_KNOWN_HOSTS` to a known_hosts file with their host keys
/// (relative to the workspace).
#[test]
#[ignore = "needs the screenshot servers (see the comment)"]
fn render_readme() {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use egui_kittest::kittest::Queryable;
    use rooste_core::{Config, MemoryStore, Profile, Theme, Tunnel, TunnelKind};
    use rooste_ssh::RoosteApp;
    use rooste_ssh::app::TabState;

    /// Typed into each session before the screenshot.
    const WEB_COMMANDS: &[&str] = &[
        "curl -sI localhost | head -n 3",
        "ls -l",
        "tail -n 3 logs/access.log",
    ];
    const DB_COMMANDS: &[&str] = &["df -h /", "free -h", "uptime"];

    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let ports: HashMap<String, u16> = std::env::var("ROOSTE_SCREENSHOT_SERVERS")
        .expect("ROOSTE_SCREENSHOT_SERVERS is not set (see the comment)")
        .split(',')
        .map(|server| {
            let (name, port) = server.split_once('=').unwrap();
            (name.to_owned(), port.parse().unwrap())
        })
        .collect();
    let known_hosts_file = workspace.join(std::env::var("ROOSTE_SCREENSHOT_KNOWN_HOSTS").unwrap());
    let key = workspace.join("crates/rooste-testkit/keys/id_ed25519");
    let server = |name: &str, folder: &str| Profile {
        name: name.into(),
        folder: folder.into(),
        host: "127.0.0.1".into(),
        port: Some(ports[name]),
        user: "deploy".into(),
        key_file: Some(key.clone()),
        use_agent: false,
        ..Profile::default()
    };
    let web1 = server("web-1", "Production");
    let db = server("db-primary", "Production");
    let staging = server("staging", "Staging");
    // Only listed (and web-2 shown in the session editor), never connected.
    let listed = |name: &str, folder: &str| Profile {
        name: name.into(),
        folder: folder.into(),
        host: format!("{}.example.com", name.to_lowercase().replace(' ', "-")),
        user: "deploy".into(),
        ..Profile::default()
    };
    let web2 = Profile {
        proxy_jump: Some("bastion.example.com".into()),
        tags: vec!["nginx".into(), "eu-west".into()],
        tunnels: vec![
            Tunnel {
                kind: TunnelKind::Local,
                listen: "5432".into(),
                target: "db.internal:5432".into(),
            },
            Tunnel {
                kind: TunnelKind::Dynamic,
                listen: "1080".into(),
                target: String::new(),
            },
        ],
        ..listed("web-2", "Production")
    };
    let out = workspace.join("target/screenshots");
    std::fs::create_dir_all(&out).unwrap();

    fn wait(harness: &mut Harness<'_, RoosteApp>, done: impl Fn(&RoosteApp) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !done(harness.state()) {
            assert!(
                Instant::now() < deadline,
                "timed out: {:?}",
                harness
                    .state()
                    .tabs()
                    .map(|t| (
                        t.name.clone(),
                        format!("{:?}", t.state),
                        t.terminal.screen_text(),
                        t.files.as_ref().map(|f| (f.entries.len(), f.error.clone()))
                    ))
                    .collect::<Vec<_>>()
            );
            harness.step();
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    /// Type `commands` into the session named `name`, each after the previous
    /// one has finished (its prompt is back).
    fn run(harness: &mut Harness<'_, RoosteApp>, name: &str, commands: &[&str]) {
        let prompt = format!("deploy@{name}:~$");
        let prompts = |app: &RoosteApp| {
            app.tabs()
                .find(|t| t.name == name)
                .map_or(0, |t| t.terminal.screen_text().matches(&prompt).count())
        };
        wait(harness, |app| prompts(app) >= 1);
        for command in commands {
            let before = prompts(harness.state());
            let tab = harness
                .state_mut()
                .tabs_mut()
                .find(|t| t.name == name)
                .unwrap();
            tab.send_input(format!("{command}\r").into_bytes());
            wait(harness, |app| prompts(app) > before);
        }
    }
    fn select_backup(tab: &mut rooste_ssh::tab::SessionTab) {
        let files = tab.files.as_mut().unwrap();
        let backup = files
            .entries
            .iter()
            .find(|e| e.name.starts_with("backup-"))
            .unwrap();
        files.selected = vec![backup.path.clone()];
    }
    fn open_files(harness: &mut Harness<'_, RoosteApp>) {
        let tab = harness
            .state_mut()
            .tabs_mut()
            .find(|t| t.name == "web-1")
            .unwrap();
        tab.toggle_files();
        wait(harness, |app| {
            app.tabs()
                .filter_map(|t| t.files.as_ref())
                .any(|f| f.entries.iter().any(|e| e.name == "deploy.sh"))
        });
        let tab = harness
            .state_mut()
            .tabs_mut()
            .find(|t| t.name == "web-1")
            .unwrap();
        select_backup(tab);
    }

    // A window connected to web-1, which has run its commands, with `setup`
    // run before rendering.
    let shot =
        |name: &str, theme: Theme, height: f32, setup: &dyn Fn(&mut Harness<'_, RoosteApp>)| {
            // Shots with the file panel need more room than the others.
            let width = if matches!(name, "split" | "light" | "files") {
                1600.0
            } else {
                1280.0
            };
            let dir = tempfile::tempdir().unwrap();
            let known_hosts = dir.path().join("known_hosts");
            std::fs::copy(&known_hosts_file, &known_hosts).unwrap();
            let mut config = Config {
                profiles: vec![
                    web1.clone(),
                    web2.clone(),
                    db.clone(),
                    staging.clone(),
                    listed("Raspberry Pi", "Home"),
                    listed("NAS", "Home"),
                ],
                ..Config::default()
            };
            config.settings.theme = theme;
            config.settings.download_dir = "/home/deploy/Downloads".into();
            let app = RoosteApp::with_config(config, None, Arc::new(MemoryStore::default()))
                .with_known_hosts(known_hosts);
            let mut harness = Harness::builder()
                .with_size([width, height])
                .build_ui_state(|ui, app: &mut RoosteApp| app.show(ui), app);
            rooste_ssh::fonts::install(&harness.ctx);
            let ctx = harness.ctx.clone();
            harness.state_mut().connect_profile(&ctx, &web1.id);
            wait(&mut harness, |app| {
                app.tabs().any(|t| t.state == TabState::Connected)
            });
            run(&mut harness, "web-1", WEB_COMMANDS);
            setup(&mut harness);
            // Keep the mouse pointer out of the picture.
            harness.event(egui::Event::PointerGone);
            for _ in 0..8 {
                harness.step();
            }
            harness
                .render()
                .expect("render with wgpu")
                .save(out.join(format!("readme-{name}.png")))
                .unwrap();
        };
    // web-1 on top with its file panel; db-primary below, with staging as a
    // background tab beside it.
    let split = |harness: &mut Harness<'_, RoosteApp>| {
        let ctx = harness.ctx.clone();
        harness.state_mut().connect_profile(&ctx, &db.id);
        harness.state_mut().connect_profile(&ctx, &staging.id);
        wait(harness, |app| {
            app.tabs()
                .filter(|t| t.state == TabState::Connected)
                .count()
                == 3
        });
        let app = harness.state_mut();
        let tile_of = |app: &RoosteApp, name: &str| {
            app.tree
                .tiles
                .iter()
                .find_map(|(tile_id, tile)| {
                    matches!(tile, egui_tiles::Tile::Pane(tab) if tab.name == name)
                        .then_some(*tile_id)
                })
                .unwrap()
        };
        let [web_tile, db_tile, staging_tile] =
            ["web-1", "db-primary", "staging"].map(|name| tile_of(app, name));
        let top = app.tree.tiles.parent_of(web_tile).unwrap();
        let bottom = app.tree.tiles.insert_tab_tile(vec![]);
        let split = app.tree.tiles.insert_vertical_tile(vec![top, bottom]);
        app.tree.root = Some(split);
        app.tree.move_tile_to_container(db_tile, bottom, 0, false);
        app.tree
            .move_tile_to_container(staging_tile, bottom, 1, false);
        for (group, tile) in [(top, web_tile), (bottom, db_tile)] {
            if let Some(egui_tiles::Tile::Container(egui_tiles::Container::Tabs(tabs))) =
                app.tree.tiles.get_mut(group)
            {
                tabs.set_active(tile);
            }
        }
        // web-1 (top) gets a bit more room than db-primary (bottom).
        if let Some(egui_tiles::Tile::Container(egui_tiles::Container::Linear(linear))) =
            app.tree.tiles.get_mut(split)
        {
            linear.shares.set_share(top, 1.25);
            linear.shares.set_share(bottom, 1.0);
        }
        for _ in 0..5 {
            harness.step();
        }
        run(harness, "db-primary", DB_COMMANDS);
        open_files(harness);
    };

    shot("split", Theme::Dark, 860.0, &split);
    shot("light", Theme::Light, 860.0, &split);
    shot("files", Theme::Dark, 720.0, &open_files);
    shot("search", Theme::Dark, 720.0, &|harness| {
        let tab = harness.state_mut().tabs_mut().next().unwrap();
        tab.open_find();
        let find = tab.find.as_mut().unwrap();
        find.query = "HTTP/1.1".into();
        find.found = Some(
            tab.terminal
                .find("HTTP/1.1", rooste_terminal::SearchDirection::Up),
        );
    });
    shot("editor", Theme::Dark, 960.0, &|harness| {
        harness.get_by_label("web-2").hover();
        harness.step();
        harness.get_by_label("web-2").click_secondary();
        harness.step();
        harness.step();
        harness.get_by_label("Edit…").click();
        harness.step();
    });
    shot("palette", Theme::Dark, 720.0, &|harness| {
        harness.key_press_modifiers(
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            egui::Key::P,
        );
        harness.step();
        harness.event(egui::Event::Text("split".into()));
        harness.step();
    });
    shot("keygen", Theme::Dark, 720.0, &|harness| {
        harness.get_by_label("Tools").click();
        for _ in 0..3 {
            harness.step();
        }
        harness.get_by_label("Key generator…").click();
        for _ in 0..3 {
            harness.step();
        }
        if let Some(rooste_ssh::app::Dialog::KeyGenerator(generator)) =
            &mut harness.state_mut().dialog
        {
            generator.comment = "deploy@laptop".into();
            generator.path = "~/.ssh/id_ed25519".into();
        }
    });
}
