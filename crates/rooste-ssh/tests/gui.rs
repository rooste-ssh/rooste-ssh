//! GUI tests with egui_kittest, against the in-process SSH server.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use rooste_core::{Config, MemoryStore, Profile, Theme};
use rooste_protocols::ssh::known_hosts;
use rooste_ssh::RoosteApp;
use rooste_ssh::app::{Dialog, TabState};
use rooste_ssh::session::PromptKind;
use rooste_ssh::tab::SessionTab;
use rooste_testkit::{TEST_CODE, TEST_PASSWORD, TEST_USER, TestServer};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    net: tokio::runtime::Runtime,
    server: Option<TestServer>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            net: tokio::runtime::Runtime::new().unwrap(),
            server: None,
        }
    }

    /// Also start an SSH server whose host key is already trusted.
    fn with_server() -> Self {
        let f = Self::new();
        let server = f.net.block_on(TestServer::start());
        f.trusting(server)
    }

    /// A server that asks for the password and a one-time code.
    fn with_keyboard_interactive_server() -> Self {
        let f = Self::new();
        let server = f.net.block_on(TestServer::start_keyboard_interactive());
        f.trusting(server)
    }

    /// Use `server`, with its host key already trusted.
    fn trusting(mut self, server: TestServer) -> Self {
        known_hosts::learn(
            &self.known_hosts(),
            "127.0.0.1",
            server.addr.port(),
            &server.host_key,
        )
        .unwrap();
        self.server = Some(server);
        self
    }

    fn start_untrusted_server(&self) -> TestServer {
        self.net.block_on(TestServer::start())
    }

    fn config_path(&self) -> PathBuf {
        self.dir.path().join("config.toml")
    }

    fn known_hosts(&self) -> PathBuf {
        self.dir.path().join("known_hosts")
    }

    fn app(&self, config: Config) -> RoosteApp {
        RoosteApp::with_config(
            config,
            Some(self.config_path()),
            Arc::new(MemoryStore::default()),
        )
        .with_known_hosts(self.known_hosts())
    }

    fn saved_config(&self) -> Config {
        Config::load(&self.config_path()).unwrap()
    }

    fn profile(&self, name: &str) -> Profile {
        Profile {
            name: name.into(),
            host: "127.0.0.1".into(),
            port: self.server.as_ref().map(|s| s.addr.port()),
            user: TEST_USER.into(),
            use_agent: false,
            ..Profile::default()
        }
    }
}

/// Render a few frames. (`Harness::run` waits for the UI to go idle, which
/// never happens while a session streams output.)
fn settle(harness: &mut Harness<'_, RoosteApp>) {
    for _ in 0..4 {
        harness.step();
    }
}

fn harness(app: RoosteApp) -> Harness<'static, RoosteApp> {
    Harness::builder()
        .with_size([1100.0, 800.0])
        .build_ui_state(|ui, app: &mut RoosteApp| app.show(ui), app)
}

fn first_tab(app: &RoosteApp) -> Option<&SessionTab> {
    app.tabs().next()
}

fn screen_text(app: &RoosteApp) -> String {
    first_tab(app)
        .map(|t| t.terminal.screen_text())
        .unwrap_or_default()
}

/// Step the UI until `done` holds, failing after a timeout.
fn wait_for(harness: &mut Harness<'_, RoosteApp>, what: &str, done: impl Fn(&RoosteApp) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(harness.state()) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; screen:\n{}\napp: {:?}",
            screen_text(harness.state()),
            harness.state()
        );
        harness.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    harness.step();
}

/// The last widget with `label`: dialogs are drawn after (and on top of)
/// the window, which may have widgets with the same label.
fn last<'a>(harness: &'a Harness<'_, RoosteApp>, label: &'a str) -> egui_kittest::Node<'a> {
    harness
        .query_all_by_label(label)
        .last()
        .unwrap_or_else(|| panic!("no widget labeled {label:?}"))
}

/// Move the pointer onto a widget, then click it, as a mouse user would.
/// (A click without a preceding hover can be ignored by egui.)
fn click(harness: &mut Harness<'_, RoosteApp>, label: &str) {
    last(harness, label).hover();
    settle(harness);
    last(harness, label).click();
    settle(harness);
}

fn type_into(harness: &mut Harness<'_, RoosteApp>, label: &str, text: &str) {
    click(harness, label);
    last(harness, label).type_text(text);
    settle(harness);
}

fn open_menu_item(harness: &mut Harness<'_, RoosteApp>, menu: &str, item: &str) {
    harness.get_by_label(menu).click();
    settle(harness);
    harness.get_by_label(item).click();
    settle(harness);
}

fn context_menu(harness: &mut Harness<'_, RoosteApp>, profile: &str, item: &str) {
    harness.get_by_label(profile).hover();
    settle(harness);
    harness.get_by_label(profile).click_secondary();
    settle(harness);
    // The welcome screen also has a "Connect" button; the menu item is the
    // one next to the session.
    let session = harness.get_by_label(profile).rect().center();
    harness
        .get_all_by_label(item)
        .min_by(|a, b| {
            let distance = |node: &egui_kittest::Node<'_>| node.rect().center().distance(session);
            distance(a).total_cmp(&distance(b))
        })
        .unwrap()
        .click();
    settle(harness);
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

// ---- Quick connect ----------------------------------------------------

#[test]
fn quick_connect_requires_a_host() {
    let f = Fixture::new();
    let mut harness = harness(f.app(Config::default()));
    harness.get_by_label("Connect").click();
    settle(&mut harness);
    harness.get_by_label("Enter a host name.");
    assert_eq!(harness.state().tabs().count(), 0);
}

#[test]
fn quick_connect_trust_host_key_type_and_exit() {
    let f = Fixture::new();
    let server = f.start_untrusted_server();

    let mut harness = harness(f.app(Config::default()));
    type_into(&mut harness, "Host", "127.0.0.1");
    {
        let quick = &mut harness.state_mut().quick;
        quick.port = server.addr.port().to_string();
        quick.user = TEST_USER.into();
        quick.password = TEST_PASSWORD.into();
        quick.use_agent = false;
    }
    assert_eq!(
        harness.state().quick.host,
        "127.0.0.1",
        "typed into the Host field"
    );
    harness.get_by_label("Connect").click();

    wait_for(&mut harness, "host key prompt", |app| {
        first_tab(app).is_some_and(|t| t.pending_host_key().is_some())
    });
    harness.get_by_label("Trust and connect").click();

    wait_for(&mut harness, "shell prompt", |app| {
        first_tab(app).is_some_and(|t| t.state == TabState::Connected)
            && screen_text(app).contains('$')
    });
    assert!(f.known_hosts().exists(), "trusted key was recorded");

    // The fake shell reports the PTY size it was told about: it must match
    // the grid the widget laid out, not the initial 80x24.
    let (cols, rows) = first_tab(harness.state()).unwrap().terminal.size();
    assert_ne!((cols, rows), (80, 24));
    harness.event(egui::Event::Text("size".into()));
    harness.key_press(egui::Key::Enter);
    let expected = format!("{cols}x{rows}");
    wait_for(&mut harness, "size reply", |app| {
        screen_text(app).contains(&expected)
    });

    harness.event(egui::Event::Text("exit 4".into()));
    harness.key_press(egui::Key::Enter);
    wait_for(&mut harness, "session end", |app| {
        first_tab(app).is_some_and(|t| t.state == TabState::Closed(Some(4)))
    });
    harness.get_by_label("Session closed (exit status 4)");
    harness.get_by_label("Reconnect");
}

#[test]
fn rejected_host_key_fails_the_connection() {
    let f = Fixture::new();
    let server = f.start_untrusted_server();

    let mut harness = harness(f.app(Config::default()));
    {
        let quick = &mut harness.state_mut().quick;
        quick.host = "127.0.0.1".into();
        quick.port = server.addr.port().to_string();
        quick.user = TEST_USER.into();
        quick.password = TEST_PASSWORD.into();
        quick.use_agent = false;
    }
    harness.get_by_label("Connect").click();
    wait_for(&mut harness, "host key prompt", |app| {
        first_tab(app).is_some_and(|t| t.pending_host_key().is_some())
    });
    harness.get_by_label("Cancel").click();
    wait_for(&mut harness, "failure", |app| {
        matches!(first_tab(app).map(|t| &t.state), Some(TabState::Failed(_)))
    });
    let Some(TabState::Failed(message)) = first_tab(harness.state()).map(|t| t.state.clone())
    else {
        unreachable!()
    };
    assert!(message.contains("not trusted"), "{message}");
}

// ---- Session manager ---------------------------------------------------

#[test]
fn create_a_profile_in_the_editor() {
    let f = Fixture::new();
    let mut harness = harness(f.app(Config::default()));
    harness.get_by_label("➕").click();
    settle(&mut harness);
    harness.get_by_label("New session");

    click(&mut harness, "Save");
    harness.get_by_label("Enter a host name.");

    type_into(&mut harness, "Name", "My server");
    type_into(&mut harness, "Host", "server.example.com");
    type_into(&mut harness, "Port", "2200");
    type_into(&mut harness, "Folder", "Work/Prod");
    harness.get_by_label("Save").click();
    settle(&mut harness);

    assert!(harness.state().dialog.is_none());
    harness.get_by_label("📁 Work");
    harness.get_by_label("My server");
    let saved = f.saved_config();
    assert_eq!(saved.profiles.len(), 1);
    let p = &saved.profiles[0];
    assert_eq!(
        (p.name.as_str(), p.host.as_str()),
        ("My server", "server.example.com")
    );
    assert_eq!(p.port, Some(2200));
    assert_eq!(p.folder, "Work/Prod");
}

#[test]
fn browse_buttons_fill_key_file_fields() {
    let f = Fixture::new();
    let key = f.dir.path().join("id_browse");
    let mut harness = harness(f.app(Config::default()));
    harness.state_mut().picker.fake_answer = Some(key.clone());

    click(&mut harness, "Browse…");
    assert_eq!(harness.state().quick.key_file, key.display().to_string());

    harness.get_by_label("➕").click();
    settle(&mut harness);
    type_into(&mut harness, "Name", "Browsed");
    type_into(&mut harness, "Host", "server.example.com");
    click(&mut harness, "Browse…");
    harness.get_by_label("Save").click();
    settle(&mut harness);
    assert!(harness.state().dialog.is_none());
    assert_eq!(f.saved_config().profiles[0].key_file, Some(key));
}

#[test]
fn right_click_in_the_session_list() {
    let f = Fixture::new();
    let config = Config {
        profiles: vec![Profile {
            folder: "Work/Prod".into(),
            ..f.profile("prod-db")
        }],
        ..Config::default()
    };
    let mut harness = harness(f.app(config));

    // The empty part of the list offers a new session, quick connect and imports.
    last(&harness, "Session list").hover();
    settle(&mut harness);
    last(&harness, "Session list").click_secondary();
    settle(&mut harness);
    harness.get_by_label("Import from ~/.ssh/config");
    click(&mut harness, "New session…");
    let Some(Dialog::EditProfile(editor)) = &harness.state().dialog else {
        panic!("the session editor opens");
    };
    assert_eq!(editor.profile.folder, "");
    click(&mut harness, "Cancel");

    // A folder offers a new session inside it.
    harness.get_by_label("📁 Prod").hover();
    settle(&mut harness);
    harness.get_by_label("📁 Prod").click_secondary();
    settle(&mut harness);
    click(&mut harness, "New session in this folder…");
    let Some(Dialog::EditProfile(editor)) = &harness.state().dialog else {
        panic!("the session editor opens");
    };
    assert_eq!(editor.profile.folder, "Work/Prod");
}

#[test]
fn search_filters_the_list() {
    let f = Fixture::new();
    let config = Config {
        profiles: vec![
            Profile {
                name: "alpha".into(),
                host: "a.example.com".into(),
                tags: vec!["prod".into()],
                ..Profile::default()
            },
            Profile {
                name: "beta".into(),
                host: "b.example.com".into(),
                ..Profile::default()
            },
        ],
        ..Config::default()
    };
    let mut harness = harness(f.app(config));
    harness.get_by_label("alpha");
    harness.get_by_label("beta");
    type_into(&mut harness, "Search", "prod");
    harness.get_by_label("alpha");
    assert!(harness.query_by_label("beta").is_none());
}

#[test]
fn edit_duplicate_and_delete_from_the_context_menu() {
    let f = Fixture::new();
    let config = Config {
        profiles: vec![Profile {
            name: "router".into(),
            host: "10.0.0.1".into(),
            ..Profile::default()
        }],
        ..Config::default()
    };
    let mut harness = harness(f.app(config));

    context_menu(&mut harness, "router", "Duplicate");
    harness.get_by_label("router (copy)");
    assert_eq!(f.saved_config().profiles.len(), 2);

    context_menu(&mut harness, "router (copy)", "Edit…");
    harness.get_by_label("Edit session");
    type_into(&mut harness, "Name", " 2");
    harness.get_by_label("Save").click();
    settle(&mut harness);
    harness.get_by_label("router (copy) 2");

    context_menu(&mut harness, "router (copy) 2", "Delete…");
    harness.get_by_label("Delete the saved session \"router (copy) 2\"?");
    harness.get_by_label("Delete").click();
    settle(&mut harness);
    assert!(harness.query_by_label("router (copy) 2").is_none());
    let saved = f.saved_config();
    assert_eq!(saved.profiles.len(), 1);
    assert_eq!(saved.profiles[0].name, "router");
}

#[test]
fn import_from_ssh_config_and_putty() {
    let f = Fixture::new();
    let ssh_config = f.dir.path().join("ssh_config");
    write(
        &ssh_config,
        "Host web\n  HostName web.example.com\nHost db\n  User admin\n",
    );
    let putty = f.dir.path().join("putty");
    std::fs::create_dir(&putty).unwrap();
    write(
        &putty.join("Switch%201"),
        "HostName=192.168.0.2\nProtocol=telnet\n",
    );

    let app = f
        .app(Config::default())
        .with_import_sources(rooste_ssh::app::ImportSources {
            putty_dir: Some(putty),
            ssh_config: Some(ssh_config),
        });
    let mut harness = harness(app);

    open_menu_item(&mut harness, "Session", "Import from ~/.ssh/config");
    harness.get_by_label("Imported 2 sessions from ~/.ssh/config.");
    harness.get_by_label("web");
    harness.get_by_label("db");

    open_menu_item(&mut harness, "Session", "Import from ~/.ssh/config");
    harness.get_by_label("Imported 0 sessions from ~/.ssh/config (2 already existed).");
    assert_eq!(harness.state().config.profiles.len(), 2, "no duplicates");

    open_menu_item(&mut harness, "Session", "Import from PuTTY");
    harness.get_by_label("Imported 1 sessions from PuTTY.");
    harness.get_by_label("Switch 1");
    assert_eq!(f.saved_config().profiles.len(), 3);

    let switch = harness
        .state()
        .config
        .profiles
        .iter()
        .find(|p| p.name == "Switch 1")
        .unwrap()
        .clone();
    assert_eq!(switch.protocol, rooste_core::Protocol::Telnet);
}

#[test]
fn settings_are_saved_and_applied() {
    let f = Fixture::new();
    let mut harness = harness(f.app(Config::default()));
    open_menu_item(&mut harness, "Session", "Settings…");
    assert!(matches!(harness.state().dialog, Some(Dialog::Settings(_))));
    harness.get_by_label("Light").click();
    settle(&mut harness);
    harness.get_by_label("Copy text when selected").click();
    settle(&mut harness);
    harness.get_by_label("Save").click();
    settle(&mut harness);

    let saved = f.saved_config();
    assert_eq!(saved.settings.theme, Theme::Light);
    assert!(!saved.settings.copy_on_select);
    assert!(
        !harness.ctx.global_style().visuals.dark_mode,
        "light visuals applied"
    );
}

// ---- Connecting saved profiles ------------------------------------------

#[test]
fn keyboard_interactive_login_asks_each_question() {
    let f = Fixture::with_keyboard_interactive_server();
    let profile = Profile {
        save_password: true,
        ..f.profile("two factor")
    };
    let id = profile.id.clone();
    let config = Config {
        profiles: vec![profile],
        ..Config::default()
    };
    let mut harness = harness(f.app(config));
    context_menu(&mut harness, "two factor", "Connect");

    // First the password (hidden), then the code, each in the prompt dialog.
    wait_for(&mut harness, "password question", |app| {
        first_tab(app).is_some_and(|t| t.prompt.is_some())
    });
    harness.get_by_label(&format!("Log in to {TEST_USER}@127.0.0.1"));
    type_into(&mut harness, "Password", TEST_PASSWORD);
    harness.get_by_label("Log in").click();
    wait_for(&mut harness, "code question", |app| {
        first_tab(app).is_some_and(|t| {
            t.prompt.as_ref().is_some_and(|p| {
                matches!(&p.kind, PromptKind::Interactive { prompt, .. } if prompt.starts_with("Verification code"))
            })
        })
    });
    harness.get_by_label("Two-factor login");
    harness.get_by_label("Enter the code from your app.");
    type_into(&mut harness, "Verification code", TEST_CODE);
    harness.get_by_label("Log in").click();
    wait_for(&mut harness, "connected", |app| {
        first_tab(app).is_some_and(|t| t.state == TabState::Connected)
    });
    assert_eq!(
        harness.state().secrets().get(&id).as_deref(),
        Some(TEST_PASSWORD),
        "the password (not the code) is saved"
    );

    // Next time the saved password answers the password question; only the
    // code is asked.
    let ctx = harness.ctx.clone();
    harness.state_mut().connect_profile(&ctx, &id);
    wait_for(&mut harness, "code question only", |app| {
        app.tabs().any(|t| {
            t.prompt.as_ref().is_some_and(|p| {
                matches!(&p.kind, PromptKind::Interactive { prompt, .. } if prompt.starts_with("Verification code"))
            })
        })
    });
    type_into(&mut harness, "Verification code", TEST_CODE);
    harness.get_by_label("Log in").click();
    wait_for(&mut harness, "both connected", |app| {
        connected_count(app) == 2
    });
}

#[test]
fn password_prompt_and_saving_the_password() {
    let f = Fixture::with_server();
    let profile = Profile {
        save_password: true,
        ..f.profile("test box")
    };
    let id = profile.id.clone();
    let config = Config {
        profiles: vec![profile],
        ..Config::default()
    };
    let mut harness = harness(f.app(config));

    context_menu(&mut harness, "test box", "Connect");
    let label = format!("Password for {TEST_USER}@127.0.0.1");
    wait_for(&mut harness, "password prompt", |app| {
        first_tab(app).is_some_and(|t| t.prompt.is_some())
    });

    // A wrong password is rejected and asked again.
    type_into(&mut harness, &label, "wrong");
    harness.get_by_label("Log in").click();
    wait_for(&mut harness, "retry prompt", |app| {
        first_tab(app).is_some_and(|t| t.prompt.as_ref().is_some_and(|p| p.retry))
    });
    harness.get_by_label("Not accepted, please try again.");

    type_into(&mut harness, &label, TEST_PASSWORD);
    harness.get_by_label("Log in").click();
    wait_for(&mut harness, "connected", |app| {
        first_tab(app).is_some_and(|t| t.state == TabState::Connected)
    });
    assert_eq!(first_tab(harness.state()).unwrap().name, "test box");
    assert_eq!(
        harness.state().secrets().get(&id).as_deref(),
        Some(TEST_PASSWORD),
        "accepted password saved for the profile"
    );
}

#[test]
fn saved_password_is_used_without_prompting() {
    let f = Fixture::with_server();
    let profile = Profile {
        save_password: true,
        ..f.profile("saved")
    };
    let id = profile.id.clone();
    let app = f.app(Config {
        profiles: vec![profile],
        ..Config::default()
    });
    app.secrets().set(&id, TEST_PASSWORD).unwrap();
    let mut harness = harness(app);
    context_menu(&mut harness, "saved", "Connect");
    wait_for(&mut harness, "connected", |app| {
        first_tab(app).is_some_and(|t| t.state == TabState::Connected)
    });
}

#[test]
fn cancelling_the_prompt_fails_the_login() {
    let f = Fixture::with_server();
    let mut harness = harness(f.app(Config {
        profiles: vec![f.profile("p")],
        ..Config::default()
    }));
    context_menu(&mut harness, "p", "Connect");
    wait_for(&mut harness, "password prompt", |app| {
        first_tab(app).is_some_and(|t| t.prompt.is_some())
    });
    harness.get_by_label("Cancel").click();
    wait_for(&mut harness, "failure", |app| {
        first_tab(app).is_some_and(|t| t.state == TabState::Failed("Login cancelled".into()))
    });
}

#[test]
fn tabs_open_side_by_side_and_close_with_confirmation() {
    let f = Fixture::with_server();
    let profiles = vec![
        Profile {
            save_password: true,
            ..f.profile("one")
        },
        Profile {
            save_password: true,
            ..f.profile("two")
        },
    ];
    let app = f.app(Config {
        profiles: profiles.clone(),
        ..Config::default()
    });
    for p in &profiles {
        app.secrets().set(&p.id, TEST_PASSWORD).unwrap();
    }
    let mut harness = harness(app);
    context_menu(&mut harness, "one", "Connect");
    context_menu(&mut harness, "two", "Connect");
    wait_for(&mut harness, "both connected", |app| {
        app.tabs().count() == 2 && app.tabs().all(|t| t.state == TabState::Connected)
    });

    // Ctrl+Shift+W asks before closing an active connection.
    harness.key_press_modifiers(
        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        egui::Key::W,
    );
    settle(&mut harness);
    harness.get_by_label("This tab has an active connection. Close it?");
    last(&harness, "Close").click();
    settle(&mut harness);
    assert_eq!(harness.state().tabs().count(), 1);
}

// ---- M4: power features --------------------------------------------------

fn connected_count(app: &RoosteApp) -> usize {
    app.tabs()
        .filter(|t| t.state == TabState::Connected)
        .count()
}

/// An app with saved-password profiles for each name, all pointing at the
/// fixture's server.
fn app_with(f: &Fixture, profiles: Vec<Profile>) -> RoosteApp {
    let profiles: Vec<Profile> = profiles
        .into_iter()
        .map(|p| Profile {
            save_password: true,
            ..p
        })
        .collect();
    let app = f.app(Config {
        profiles: profiles.clone(),
        ..Config::default()
    });
    for p in &profiles {
        app.secrets().set(&p.id, TEST_PASSWORD).unwrap();
    }
    app
}

fn tab_text(app: &RoosteApp, index: usize) -> String {
    app.tabs()
        .nth(index)
        .map(|t| t.terminal.screen_text())
        .unwrap_or_default()
}

#[test]
fn tab_right_click_menu_duplicates_and_closes() {
    let f = Fixture::with_server();
    let mut harness = harness(app_with(&f, vec![f.profile("one")]));
    context_menu(&mut harness, "one", "Connect");
    wait_for(&mut harness, "connected", |app| connected_count(app) == 1);

    // The tab is the last widget labeled "one" (the session list comes first).
    let tab_menu = |harness: &mut Harness<'_, RoosteApp>, item: &str| {
        last(harness, "one").hover();
        settle(harness);
        last(harness, "one").click_secondary();
        settle(harness);
        last(harness, item).click();
        settle(harness);
    };
    tab_menu(&mut harness, "Duplicate tab");
    wait_for(&mut harness, "both connected", |app| {
        connected_count(app) == 2
    });

    harness.state_mut().config.settings.confirm_close = false;
    tab_menu(&mut harness, "Close tab");
    wait_for(&mut harness, "one tab left", |app| app.tabs().count() == 1);
}

#[test]
fn ctrl_plus_minus_and_zero_zoom_the_active_tab() {
    let f = Fixture::with_server();
    let mut harness = harness(app_with(&f, vec![f.profile("one")]));
    context_menu(&mut harness, "one", "Connect");
    wait_for(&mut harness, "connected", |app| connected_count(app) == 1);
    wait_for(&mut harness, "prompt", |app| screen_text(app).contains('$'));
    let zoom = |harness: &Harness<'_, RoosteApp>| first_tab(harness.state()).unwrap().view.zoom;
    let cols =
        |harness: &Harness<'_, RoosteApp>| first_tab(harness.state()).unwrap().terminal.size().0;
    let before = cols(&harness);

    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Equals);
    settle(&mut harness);
    assert!(zoom(&harness) > 1.0);
    assert!(cols(&harness) < before, "larger text fits fewer columns");

    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Minus);
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Minus);
    settle(&mut harness);
    assert!(zoom(&harness) < 1.0);

    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Num0);
    settle(&mut harness);
    assert_eq!(zoom(&harness), 1.0);
    assert_eq!(cols(&harness), before);

    // The zoom keys never reached the shell (Ctrl - would send ^_).
    harness.event(egui::Event::Text("after".into()));
    harness.key_press(egui::Key::Enter);
    wait_for(&mut harness, "echo", |app| {
        screen_text(app).contains("echo: after")
    });
    assert!(!screen_text(harness.state()).contains("echo: \u{1f}"));
}

#[test]
fn find_bar_searches_the_terminal_output() {
    let f = Fixture::with_server();
    let mut harness = harness(app_with(&f, vec![f.profile("one")]));
    context_menu(&mut harness, "one", "Connect");
    wait_for(&mut harness, "connected", |app| connected_count(app) == 1);
    wait_for(&mut harness, "prompt", |app| screen_text(app).contains('$'));
    for word in ["alpha", "beta", "alpha again"] {
        harness.event(egui::Event::Text(word.into()));
        harness.key_press(egui::Key::Enter);
        settle(&mut harness);
    }
    wait_for(&mut harness, "output", |app| {
        screen_text(app).contains("echo: alpha again")
    });
    let selected = |harness: &Harness<'_, RoosteApp>| {
        first_tab(harness.state())
            .unwrap()
            .terminal
            .selection_text()
    };

    // Ctrl+Shift+F opens the bar; typing finds the newest match.
    harness.key_press_modifiers(
        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        egui::Key::F,
    );
    settle(&mut harness);
    last(&harness, "Find").type_text("echo: alpha");
    settle(&mut harness);
    assert_eq!(selected(&harness).as_deref(), Some("echo: alpha"));

    // Enter goes to the older match, and the bar keeps focus.
    harness.key_press(egui::Key::Enter);
    settle(&mut harness);
    assert_eq!(selected(&harness).as_deref(), Some("echo: alpha"));
    assert!(
        harness
            .state()
            .tabs()
            .next()
            .unwrap()
            .find
            .as_ref()
            .unwrap()
            .found
            == Some(true)
    );

    // Text that isn't there says so.
    last(&harness, "Find").type_text("zzz");
    settle(&mut harness);
    harness.get_by_label("Not found");
    assert_eq!(selected(&harness), None);

    // Esc closes the bar.
    harness.key_press(egui::Key::Escape);
    settle(&mut harness);
    assert!(first_tab(harness.state()).unwrap().find.is_none());
    assert!(harness.query_by_label("Not found").is_none());
}

#[test]
fn sessions_are_logged_as_plain_text() {
    let f = Fixture::with_server();
    let logs = f.dir.path().join("logs");
    let mut app = app_with(&f, vec![f.profile("one")]);
    app.config.settings.log_dir = logs.display().to_string();
    let mut harness = harness(app);
    context_menu(&mut harness, "one", "Connect");
    wait_for(&mut harness, "connected", |app| connected_count(app) == 1);
    wait_for(&mut harness, "prompt", |app| screen_text(app).contains('$'));
    let read_log = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
    let auto_log = first_tab(harness.state())
        .unwrap()
        .log
        .as_ref()
        .unwrap()
        .path
        .clone();
    assert!(auto_log.starts_with(&logs));
    assert!(first_tab(harness.state()).unwrap().title().contains('⏺'));

    harness.event(egui::Event::Text("hello log".into()));
    harness.key_press(egui::Key::Enter);
    wait_for(&mut harness, "logged", |_| {
        read_log(&auto_log).contains("echo: hello log\n")
    });
    let text = read_log(&auto_log);
    assert!(text.starts_with("=== one, "), "{text}");
    assert!(!text.contains('\x1b') && !text.contains('\r'), "{text:?}");

    // Stop logging: later output stays out of the file.
    open_menu_item(&mut harness, "Tab", "Stop logging");
    harness.event(egui::Event::Text("not logged".into()));
    harness.key_press(egui::Key::Enter);
    wait_for(&mut harness, "echo", |app| {
        screen_text(app).contains("echo: not logged")
    });
    assert!(!read_log(&auto_log).contains("not logged"));

    // Start again by hand, choosing the file.
    let manual = f.dir.path().join("manual.log");
    harness.state_mut().picker.fake_answer = Some(manual.clone());
    open_menu_item(&mut harness, "Tab", "Start logging…");
    settle(&mut harness);
    harness.event(egui::Event::Text("by hand".into()));
    harness.key_press(egui::Key::Enter);
    wait_for(&mut harness, "manual log", |_| {
        read_log(&manual).contains("echo: by hand")
    });
}

#[test]
fn sessions_can_be_recorded_for_playback() {
    let f = Fixture::with_server();
    let mut harness = harness(app_with(&f, vec![f.profile("one")]));
    context_menu(&mut harness, "one", "Connect");
    wait_for(&mut harness, "connected", |app| connected_count(app) == 1);
    wait_for(&mut harness, "prompt", |app| screen_text(app).contains('$'));

    let cast = f.dir.path().join("one.cast");
    harness.state_mut().picker.fake_answer = Some(cast.clone());
    open_menu_item(&mut harness, "Tab", "Record session…");
    settle(&mut harness);
    assert!(first_tab(harness.state()).unwrap().title().contains('🔘'));
    harness.event(egui::Event::Text("on tape".into()));
    harness.key_press(egui::Key::Enter);
    let read = || std::fs::read_to_string(&cast).unwrap_or_default();
    wait_for(&mut harness, "recorded", |_| {
        read().contains("echo: on tape")
    });

    let text = read();
    let mut lines = text.lines();
    let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["version"], 2);
    assert_eq!(header["title"], "one");
    let events: Vec<serde_json::Value> = lines.map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(events.iter().all(|e| e[1] == "o" || e[1] == "r"));
    // What the user typed is not recorded as input; only the echo is output.
    assert!(!events.iter().any(|e| e[1] == "i"));

    open_menu_item(&mut harness, "Tab", "Stop recording");
    assert!(first_tab(harness.state()).unwrap().recording.is_none());
}

#[test]
fn scripts_drive_the_session() {
    let f = Fixture::with_server();
    let scripts = f.dir.path().join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::write(
        scripts.join("greet.rhai"),
        r#"
        send_line("scripted");
        if wait_for("echo: scripted", 5) {
            let name = ask("Your name?");
            send_line(`hi ${name}`);
        }
        "#,
    )
    .unwrap();
    std::fs::write(scripts.join("hello.rhai"), r#"send_line(`auto ${USER}`);"#).unwrap();
    let profile = Profile {
        on_connect_script: "hello.rhai".into(),
        ..f.profile("one")
    };
    let mut harness = harness(app_with(&f, vec![profile]));

    // "Run on connect" runs as soon as the session is up.
    context_menu(&mut harness, "one", "Connect");
    wait_for(&mut harness, "on-connect script", |app| {
        screen_text(app).contains(&format!("echo: auto {TEST_USER}"))
    });

    // A script from the command palette sends, waits and asks.
    harness.key_press_modifiers(
        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        egui::Key::P,
    );
    settle(&mut harness);
    last(&harness, "Command").type_text("greet");
    settle(&mut harness);
    click(&mut harness, "Run script: greet.rhai");
    wait_for(&mut harness, "question", |app| {
        first_tab(app).is_some_and(|t| t.prompt.is_some())
    });
    harness.get_by_label("Script greet.rhai");
    type_into(&mut harness, "Your name?", "Martin");
    click(&mut harness, "OK");
    wait_for(&mut harness, "answer sent", |app| {
        screen_text(app).contains("echo: hi Martin")
    });
    wait_for(&mut harness, "finished", |app| {
        app.notice
            .as_ref()
            .is_some_and(|n| n.text == "Script greet.rhai finished.")
    });
    assert!(first_tab(harness.state()).unwrap().script.is_none());
}

#[test]
fn split_right_opens_a_second_session_beside_the_first() {
    let f = Fixture::with_server();
    let mut harness = harness(app_with(&f, vec![f.profile("one")]));
    context_menu(&mut harness, "one", "Connect");
    wait_for(&mut harness, "connected", |app| connected_count(app) == 1);

    open_menu_item(&mut harness, "Tab", "Split right");
    wait_for(&mut harness, "both connected", |app| {
        connected_count(app) == 2
    });
    let root = harness.state().tree.root.unwrap();
    assert!(
        matches!(
            harness.state().tree.tiles.get(root),
            Some(egui_tiles::Tile::Container(egui_tiles::Container::Linear(
                _
            )))
        ),
        "the root is now a horizontal split"
    );
}

#[test]
fn a_new_split_takes_the_keyboard_focus() {
    let f = Fixture::with_server();
    let mut harness = harness(app_with(&f, vec![f.profile("one")]));
    context_menu(&mut harness, "one", "Connect");
    wait_for(&mut harness, "connected", |app| connected_count(app) == 1);

    open_menu_item(&mut harness, "Tab", "Split down");
    wait_for(&mut harness, "both connected", |app| {
        connected_count(app) == 2
    });
    settle(&mut harness);

    assert_eq!(
        split_focus(harness.state()),
        (false, true),
        "the new tab has the focus"
    );
}

/// Whether the selected tab of the top and the bottom half of a split has
/// the keyboard focus.
fn split_focus(app: &RoosteApp) -> (bool, bool) {
    let Some(egui_tiles::Tile::Container(egui_tiles::Container::Linear(split))) =
        app.tree.tiles.get(app.tree.root.unwrap())
    else {
        panic!("the root is a split");
    };
    let focused = |group| match app.tree.tiles.get(group) {
        Some(egui_tiles::Tile::Container(egui_tiles::Container::Tabs(tabs))) => {
            app.tree
                .tiles
                .get_pane(&tabs.active.unwrap())
                .unwrap()
                .view
                .has_focus
        }
        _ => panic!("a tab group"),
    };
    (focused(split.children[0]), focused(split.children[1]))
}

#[test]
fn switching_tabs_keeps_the_focus_out_of_the_other_half() {
    let f = Fixture::with_server();
    let mut harness = harness(app_with(&f, vec![f.profile("one")]));
    context_menu(&mut harness, "one", "Connect");
    wait_for(&mut harness, "connected", |app| connected_count(app) == 1);
    open_menu_item(&mut harness, "Tab", "Split down");
    wait_for(&mut harness, "both connected", |app| {
        connected_count(app) == 2
    });
    settle(&mut harness);
    assert_eq!(split_focus(harness.state()), (false, true));

    // A second tab in the bottom half, from the bottom tab's menu.
    let bottom_tabs = |harness: &Harness<'_, RoosteApp>| {
        let mut rects: Vec<egui::Rect> = harness
            .get_all_by_label("one")
            .map(|node| node.rect())
            .collect();
        let bottom = rects.iter().map(|r| r.center().y).fold(f32::MIN, f32::max);
        rects.retain(|r| (r.center().y - bottom).abs() < 1.0);
        rects.sort_by(|a, b| a.left().total_cmp(&b.left()));
        rects
    };
    let first = bottom_tabs(&harness)[0].center();
    harness.event(egui::Event::PointerMoved(first));
    settle(&mut harness);
    for pressed in [true, false] {
        harness.event(egui::Event::PointerButton {
            pos: first,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: egui::Modifiers::default(),
        });
    }
    settle(&mut harness);
    harness.get_by_label("Duplicate tab").click();
    wait_for(&mut harness, "three connected", |app| {
        connected_count(app) == 3
    });
    settle(&mut harness);
    assert_eq!(bottom_tabs(&harness).len(), 2);
    assert_eq!(split_focus(harness.state()), (false, true));

    // Press on the other bottom tab and hold, then release: the press takes
    // the focus from the bottom terminal and the release switches tabs, and
    // the top terminal must not grab the focus in between.
    let pos = bottom_tabs(&harness)[0].center();
    harness.event(egui::Event::PointerMoved(pos));
    harness.event(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::default(),
    });
    for _ in 0..6 {
        harness.step();
        assert!(
            !split_focus(harness.state()).0,
            "the top tab took the focus"
        );
    }
    harness.event(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::default(),
    });
    for _ in 0..6 {
        harness.step();
        assert!(
            !split_focus(harness.state()).0,
            "the top tab took the focus"
        );
    }
    assert_eq!(split_focus(harness.state()), (false, true));
}

/// A connected session with 500 lines of scrollback.
fn long_session(f: &Fixture) -> Harness<'static, RoosteApp> {
    let mut harness = harness(app_with(f, vec![f.profile("one")]));
    context_menu(&mut harness, "one", "Connect");
    wait_for(&mut harness, "connected", |app| connected_count(app) == 1);
    let tab = harness.state_mut().tabs_mut().next().unwrap();
    for i in 0..500 {
        tab.terminal.advance(format!("line {i}\r\n").as_bytes());
    }
    settle(&mut harness);
    harness
}

fn scrolled_back(harness: &Harness<'_, RoosteApp>) -> usize {
    first_tab(harness.state())
        .unwrap()
        .terminal
        .display_offset()
}

#[test]
fn a_mouse_wheel_notch_scrolls_three_lines() {
    let f = Fixture::with_server();
    let mut harness = long_session(&f);
    harness.get_by_label("Terminal").hover();
    settle(&mut harness);
    harness.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Line,
        delta: egui::vec2(0.0, 1.0),
        phase: egui::TouchPhase::Move,
        source: egui::MouseWheelSource::Wheel,
        modifiers: egui::Modifiers::default(),
    });
    settle(&mut harness);
    assert_eq!(scrolled_back(&harness), 3);
}

#[test]
fn the_scrollbar_jumps_and_drags_through_the_scrollback() {
    let f = Fixture::with_server();
    let mut harness = long_session(&f);
    let bar = harness.get_by_label("Scrollback").rect();
    let history = first_tab(harness.state()).unwrap().terminal.history_size();
    assert!(history > 400);
    let press = |harness: &mut Harness<'_, RoosteApp>, pos: egui::Pos2, pressed: bool| {
        harness.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        });
        settle(harness);
    };

    // A click at the top of the track jumps to the oldest line.
    let top = egui::pos2(bar.center().x, bar.top() + 3.0);
    harness.event(egui::Event::PointerMoved(top));
    settle(&mut harness);
    press(&mut harness, top, true);
    assert_eq!(scrolled_back(&harness), history);

    // Dragging the thumb to the bottom returns to the newest line.
    let bottom = egui::pos2(bar.center().x, bar.bottom() - 1.0);
    harness.event(egui::Event::PointerMoved(bottom));
    settle(&mut harness);
    press(&mut harness, bottom, false);
    assert_eq!(scrolled_back(&harness), 0);
    assert!(first_tab(harness.state()).unwrap().view.has_focus);
}

#[test]
fn broadcast_input_reaches_every_tab() {
    let f = Fixture::with_server();
    let mut harness = harness(app_with(&f, vec![f.profile("one"), f.profile("two")]));
    context_menu(&mut harness, "one", "Connect");
    context_menu(&mut harness, "two", "Connect");
    wait_for(&mut harness, "both connected", |app| {
        connected_count(app) == 2
    });

    open_menu_item(&mut harness, "Tab", "Broadcast input to all tabs");
    harness.get_by_label("Broadcasting input to 2 connected tabs.");
    harness.event(egui::Event::Text("hello all".into()));
    harness.key_press(egui::Key::Enter);
    wait_for(&mut harness, "echo in both tabs", |app| {
        tab_text(app, 0).contains("echo: hello all") && tab_text(app, 1).contains("echo: hello all")
    });
}

#[test]
fn command_palette_connects_a_session() {
    let f = Fixture::with_server();
    let mut harness = harness(app_with(&f, vec![f.profile("palette target")]));
    harness.key_press_modifiers(
        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        egui::Key::P,
    );
    settle(&mut harness);
    harness.get_by_label("Command").type_text("conn palette");
    settle(&mut harness);
    harness.get_by_label("Connect: palette target");
    harness.key_press(egui::Key::Enter);
    settle(&mut harness);
    wait_for(&mut harness, "connected", |app| connected_count(app) == 1);
    assert!(harness.state().dialog.is_none());
}

#[test]
fn dropped_connections_reconnect_automatically() {
    let f = Fixture::with_server();
    let mut harness = harness(app_with(&f, vec![f.profile("flaky")]));
    context_menu(&mut harness, "flaky", "Connect");
    wait_for(&mut harness, "connected", |app| connected_count(app) == 1);

    f.net
        .block_on(f.server.as_ref().unwrap().drop_connections());
    wait_for(&mut harness, "reconnecting", |app| {
        first_tab(app).is_some_and(|t| matches!(t.state, TabState::Reconnecting { .. }))
    });
    wait_for(&mut harness, "reconnected", |app| {
        connected_count(app) == 1 && screen_text(app).contains("[reconnected]")
    });
}

#[test]
fn legacy_only_server_offers_a_retry_with_legacy_algorithms() {
    use std::borrow::Cow;
    let f = Fixture::new();
    let legacy = russh::Preferred {
        kex: Cow::Borrowed(&[russh::kex::DH_G14_SHA1]),
        ..russh::Preferred::default()
    };
    let server = f.net.block_on(TestServer::start_with(legacy));
    known_hosts::learn(
        &f.known_hosts(),
        "127.0.0.1",
        server.addr.port(),
        &server.host_key,
    )
    .unwrap();
    let profile = Profile {
        port: Some(server.addr.port()),
        ..f.profile("old switch")
    };
    let id = profile.id.clone();
    let mut harness = harness(app_with(&f, vec![profile]));
    context_menu(&mut harness, "old switch", "Connect");
    wait_for(&mut harness, "failure", |app| {
        matches!(first_tab(app).map(|t| &t.state), Some(TabState::Failed(_)))
    });
    harness
        .get_by_label("Allow legacy algorithms and retry")
        .click();
    settle(&mut harness);
    wait_for(&mut harness, "connected with legacy", |app| {
        connected_count(app) == 1
    });
    assert!(first_tab(harness.state()).unwrap().title().contains('⚠'));
    assert!(
        f.saved_config().profile(&id).unwrap().allow_legacy,
        "saved on the profile"
    );
}

#[test]
fn saved_tunnels_start_with_the_session() {
    use std::io::{Read, Write};
    let f = Fixture::with_server();
    let echo = f.net.block_on(rooste_testkit::EchoServer::start());
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = free.local_addr().unwrap().port();
    drop(free);
    // A port another socket is listening on cannot be bound again.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken_port = taken.local_addr().unwrap().port();
    let profile = Profile {
        tunnels: vec![
            rooste_core::Tunnel {
                kind: rooste_core::TunnelKind::Local,
                listen: listen.to_string(),
                target: echo.addr.to_string(),
            },
            rooste_core::Tunnel {
                kind: rooste_core::TunnelKind::Local,
                listen: taken_port.to_string(),
                target: "127.0.0.1:1".into(),
            },
        ],
        ..f.profile("tunnels")
    };
    let mut harness = harness(app_with(&f, vec![profile]));
    context_menu(&mut harness, "tunnels", "Connect");
    wait_for(&mut harness, "tunnel status", |app| {
        first_tab(app).is_some_and(|t| t.tunnels.len() == 2)
    });
    let tunnels = first_tab(harness.state()).unwrap().tunnels.clone();
    assert!(tunnels[0].result.is_ok(), "{tunnels:?}");
    assert!(
        tunnels[1].result.is_err(),
        "port 1 cannot be bound: {tunnels:?}"
    );

    let mut stream = std::net::TcpStream::connect(("127.0.0.1", listen)).unwrap();
    stream.write_all(b"via tunnel").unwrap();
    let mut back = [0u8; 10];
    stream.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"via tunnel");
}

#[test]
fn jump_host_and_agent_forwarding() {
    let f = Fixture::with_server();
    let target = f.net.block_on(TestServer::start());
    known_hosts::learn(
        &f.known_hosts(),
        "127.0.0.1",
        target.addr.port(),
        &target.host_key,
    )
    .unwrap();
    let agent = f.net.block_on(rooste_testkit::TestAgent::start());
    let profile = Profile {
        port: Some(target.addr.port()),
        proxy_jump: Some(format!(
            "{TEST_USER}@127.0.0.1:{}",
            f.server.as_ref().unwrap().addr.port()
        )),
        use_agent: true,
        forward_agent: true,
        ..f.profile("behind bastion")
    };
    let app = f
        .app(Config {
            profiles: vec![profile],
            ..Config::default()
        })
        .with_agent(rooste_protocols::ssh::AgentSource::Path(
            agent.path().to_owned(),
        ));
    let mut harness = harness(app);
    context_menu(&mut harness, "behind bastion", "Connect");
    wait_for(&mut harness, "connected through the jump host", |app| {
        connected_count(app) == 1 && screen_text(app).contains('$')
    });
    harness.event(egui::Event::Text("agent-keys".into()));
    harness.key_press(egui::Key::Enter);
    wait_for(&mut harness, "forwarded agent", |app| {
        screen_text(app).contains("agent: 1 keys")
    });
}

#[test]
fn tunnel_editor_validates() {
    let f = Fixture::new();
    let mut harness = harness(f.app(Config::default()));
    harness.get_by_label("➕").click();
    settle(&mut harness);
    type_into(&mut harness, "Host", "h.example.com");
    harness.get_by_label("Add tunnel").click();
    settle(&mut harness);
    type_into(&mut harness, "Listen", "8080");
    type_into(&mut harness, "Target", "nohost");
    harness.get_by_label("Save").click();
    settle(&mut harness);
    assert!(
        harness.state().dialog.is_some(),
        "invalid tunnel keeps the editor open"
    );
    assert!(
        harness
            .query_all_by_label("Tunnel L 8080 -> nohost: `nohost` has an invalid port")
            .next()
            .is_some()
    );
    type_into(&mut harness, "Target", ":5432");
    harness.get_by_label("Save").click();
    settle(&mut harness);
    assert!(harness.state().dialog.is_none());
    let saved = f.saved_config();
    assert_eq!(saved.profiles[0].tunnels[0].target, "nohost:5432");
}

// ---- Updates -----------------------------------------------------------

/// Serve `body` as the latest-release JSON to every request; returns the URL.
fn release_api(body: &'static str) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/releases/latest", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for socket in listener.incoming().flatten() {
            let mut socket = socket;
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes());
        }
    });
    url
}

const NEWER: &str =
    r#"{"tag_name":"v9.9.9","html_url":"https://example.com/releases/9.9.9","assets":[]}"#;

#[test]
fn new_version_banner_can_be_skipped() {
    let f = Fixture::new();
    let api = release_api(NEWER);
    let app = f
        .app(Config::default())
        .with_update_api(api.clone())
        .with_update_check();
    let mut harness = harness(app);
    wait_for(&mut harness, "banner", |app| app.update_banner.is_some());
    settle(&mut harness);
    harness.get_by_label(&format!(
        "Rooste SSH 9.9.9 is available (you have {}).",
        env!("CARGO_PKG_VERSION")
    ));
    click(&mut harness, "Skip this version");
    assert!(harness.state().update_banner.is_none());
    assert_eq!(f.saved_config().settings.skipped_version, "9.9.9");

    // At the next start the skipped version is not announced again...
    let app = f
        .app(f.saved_config())
        .with_update_api(api)
        .with_update_check();
    let mut harness = self::harness(app);
    for _ in 0..50 {
        harness.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(harness.state().update_banner.is_none());

    // ...but checking by hand still shows it.
    open_menu_item(&mut harness, "Help", "Check for updates");
    wait_for(&mut harness, "banner again", |app| {
        app.update_banner.is_some()
    });
}

#[test]
fn checking_by_hand_reports_up_to_date() {
    let f = Fixture::new();
    let current = Box::leak(
        format!(
            r#"{{"tag_name":"v{}","html_url":"https://example.com/r","assets":[]}}"#,
            env!("CARGO_PKG_VERSION")
        )
        .into_boxed_str(),
    );
    let app = f
        .app(Config::default())
        .with_update_api(release_api(current));
    let mut harness = harness(app);
    open_menu_item(&mut harness, "Help", "Check for updates");
    wait_for(&mut harness, "up to date", |app| {
        app.notice
            .as_ref()
            .is_some_and(|n| n.text.starts_with("You have the latest version"))
    });
    assert!(harness.state().update_banner.is_none());
}

// ---- M5: built-in tools ----------------------------------------------------

/// A file dropped onto the window (egui only defines the trait).
#[derive(Debug)]
struct Dropped(PathBuf);

impl egui::DroppedFile for Dropped {
    fn path(&self) -> &Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|e| e.to_string())
    }
}

fn files_entries(app: &RoosteApp) -> Vec<String> {
    first_tab(app)
        .and_then(|t| t.files.as_ref())
        .map(|f| f.entries.iter().map(|e| e.name.clone()).collect())
        .unwrap_or_default()
}

fn files_panel(app: &mut RoosteApp) -> &mut rooste_ssh::files::FilesPanel {
    app.tabs_mut()
        .next()
        .and_then(|t| t.files.as_mut())
        .unwrap()
}

#[test]
fn file_panel_browse_transfer_and_manage() {
    let f = Fixture::with_server();
    let root = f.server.as_ref().unwrap().sftp_root().to_owned();
    std::fs::create_dir(root.join("logs")).unwrap();
    std::fs::write(root.join("logs").join("app.log"), "log line\n").unwrap();
    std::fs::write(root.join("notes.txt"), "remote notes").unwrap();
    let downloads = f.dir.path().join("downloads");
    std::fs::create_dir(&downloads).unwrap();
    let mut config = Config::default();
    config.settings.download_dir = downloads.display().to_string();
    config.profiles = vec![Profile {
        save_password: true,
        ..f.profile("files")
    }];
    let id = config.profiles[0].id.clone();
    let app = f.app(config);
    app.secrets().set(&id, TEST_PASSWORD).unwrap();
    let mut harness = harness(app);
    context_menu(&mut harness, "files", "Connect");
    wait_for(&mut harness, "connected", |app| connected_count(app) == 1);

    open_menu_item(&mut harness, "Tab", "Files (SFTP)");
    wait_for(&mut harness, "listing", |app| {
        files_entries(app) == ["logs", "notes.txt"]
    });

    // Select a file and download it with the toolbar button.
    click(&mut harness, "📄 notes.txt  (12 B)");
    click(&mut harness, "Download");
    // The file appears before its contents are written; wait for all of it.
    wait_for(&mut harness, "download", |_| {
        std::fs::read_to_string(downloads.join("notes.txt")).is_ok_and(|t| t == "remote notes")
    });

    // Open a folder.
    click(&mut harness, "📁 logs");
    click(&mut harness, "Open");
    settle(&mut harness);
    wait_for(&mut harness, "logs folder", |app| {
        files_entries(app) == ["app.log"]
    });
    click(&mut harness, "⬆");
    wait_for(&mut harness, "back up", |app| files_entries(app).len() == 2);

    // Upload by dropping a file onto the window.
    let local = f.dir.path().join("upload.txt");
    std::fs::write(&local, "from here").unwrap();
    harness
        .input_mut()
        .dropped_files
        .push(Arc::new(Dropped(local.clone())));
    settle(&mut harness);
    wait_for(&mut harness, "upload", |app| {
        files_entries(app).contains(&"upload.txt".to_owned())
    });
    assert_eq!(
        std::fs::read_to_string(root.join("upload.txt")).unwrap(),
        "from here"
    );

    // New folder, rename and delete.
    click(&mut harness, "New folder");
    type_into(&mut harness, "Folder name", "made-here");
    click(&mut harness, "Create");
    wait_for(&mut harness, "mkdir", |app| {
        root.join("made-here").is_dir() && files_entries(app).contains(&"made-here".to_owned())
    });

    click(&mut harness, "📄 upload.txt  (9 B)");
    click(&mut harness, "Rename");
    type_into(&mut harness, "New name", "-2");
    click(&mut harness, "Rename");
    wait_for(&mut harness, "rename", |app| {
        root.join("upload.txt-2").exists()
            && files_entries(app).contains(&"upload.txt-2".to_owned())
    });

    click(&mut harness, "📄 upload.txt-2  (9 B)");
    click(&mut harness, "Delete");
    harness.get_by_label("Delete upload.txt-2?");
    click(&mut harness, "Delete");
    wait_for(&mut harness, "delete", |_| {
        !root.join("upload.txt-2").exists()
    });
}

#[test]
fn file_panel_downloads_folders_and_selections_to_a_chosen_folder() {
    let f = Fixture::with_server();
    let root = f.server.as_ref().unwrap().sftp_root().to_owned();
    std::fs::create_dir_all(root.join("logs").join("old")).unwrap();
    std::fs::write(root.join("logs").join("app.log"), "log line\n").unwrap();
    std::fs::write(root.join("logs").join("old").join("1.log"), "older\n").unwrap();
    std::fs::write(root.join("a.txt"), "aaa").unwrap();
    std::fs::write(root.join("b.txt"), "bbb").unwrap();
    std::fs::write(root.join("c.txt"), "ccc").unwrap();
    let downloads = f.dir.path().join("downloads");
    let chosen = f.dir.path().join("chosen");
    std::fs::create_dir(&downloads).unwrap();
    std::fs::create_dir(&chosen).unwrap();
    let mut config = Config::default();
    config.settings.download_dir = downloads.display().to_string();
    config.profiles = vec![Profile {
        save_password: true,
        ..f.profile("files")
    }];
    let id = config.profiles[0].id.clone();
    let app = f.app(config);
    app.secrets().set(&id, TEST_PASSWORD).unwrap();
    let mut harness = harness(app);
    context_menu(&mut harness, "files", "Connect");
    wait_for(&mut harness, "connected", |app| connected_count(app) == 1);
    open_menu_item(&mut harness, "Tab", "Files (SFTP)");
    wait_for(&mut harness, "listing", |app| files_entries(app).len() == 4);

    // A folder downloads with everything in it.
    click(&mut harness, "📁 logs");
    click(&mut harness, "Download");
    wait_for(&mut harness, "folder download", |_| {
        std::fs::read_to_string(downloads.join("logs").join("old").join("1.log"))
            .is_ok_and(|t| t == "older\n")
            && std::fs::read_to_string(downloads.join("logs").join("app.log"))
                .is_ok_and(|t| t == "log line\n")
    });

    // Ctrl-click adds to the selection; "Download to…" asks for a folder.
    click(&mut harness, "📄 a.txt  (3 B)");
    last(&harness, "📄 c.txt  (3 B)").click_modifiers(egui::Modifiers::COMMAND);
    settle(&mut harness);
    assert_eq!(files_panel(harness.state_mut()).selected.len(), 2);
    files_panel(harness.state_mut()).picker.fake_answer = Some(chosen.clone());
    click(&mut harness, "Download to…");
    wait_for(&mut harness, "download to", |_| {
        std::fs::read_to_string(chosen.join("a.txt")).is_ok_and(|t| t == "aaa")
            && std::fs::read_to_string(chosen.join("c.txt")).is_ok_and(|t| t == "ccc")
    });
    assert!(!chosen.join("b.txt").exists(), "b.txt was not selected");
    assert_eq!(
        f.saved_config().settings.last_download_to,
        chosen.display().to_string(),
        "the chosen folder is remembered"
    );

    // Shift-click selects a range; the remembered folder is in the menu.
    click(&mut harness, "📄 a.txt  (3 B)");
    last(&harness, "📄 c.txt  (3 B)").click_modifiers(egui::Modifiers::SHIFT);
    settle(&mut harness);
    assert_eq!(files_panel(harness.state_mut()).selected.len(), 3);
    last(&harness, "📄 b.txt  (3 B)").hover();
    settle(&mut harness);
    last(&harness, "📄 b.txt  (3 B)").click_secondary();
    settle(&mut harness);
    last(&harness, "Download to chosen").click();
    settle(&mut harness);
    wait_for(&mut harness, "download to the remembered folder", |_| {
        std::fs::read_to_string(chosen.join("b.txt")).is_ok_and(|t| t == "bbb")
            && chosen.join("a (1).txt").exists()
    });
}

#[test]
fn file_panel_uploads_folders() {
    let f = Fixture::with_server();
    let root = f.server.as_ref().unwrap().sftp_root().to_owned();
    let project = f.dir.path().join("project");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(project.join("README"), "read me").unwrap();
    std::fs::write(project.join("src").join("main.rs"), "fn main() {}").unwrap();
    let config = Config {
        profiles: vec![Profile {
            save_password: true,
            ..f.profile("files")
        }],
        ..Config::default()
    };
    let id = config.profiles[0].id.clone();
    let app = f.app(config);
    app.secrets().set(&id, TEST_PASSWORD).unwrap();
    let mut harness = harness(app);
    context_menu(&mut harness, "files", "Connect");
    wait_for(&mut harness, "connected", |app| connected_count(app) == 1);
    open_menu_item(&mut harness, "Tab", "Files (SFTP)");
    wait_for(&mut harness, "listing", |app| {
        first_tab(app)
            .and_then(|t| t.files.as_ref())
            .is_some_and(|f| f.handle.is_some())
    });
    settle(&mut harness);

    // Dropping a folder uploads it with everything in it.
    harness
        .input_mut()
        .dropped_files
        .push(Arc::new(Dropped(project.clone())));
    settle(&mut harness);
    wait_for(&mut harness, "folder upload", |app| {
        std::fs::read_to_string(root.join("project").join("src").join("main.rs"))
            .is_ok_and(|t| t == "fn main() {}")
            && files_entries(app).contains(&"project".to_owned())
    });
    assert_eq!(
        std::fs::read_to_string(root.join("project").join("README")).unwrap(),
        "read me"
    );

    // "Upload folder…" merges into the existing remote folder.
    std::fs::write(project.join("src").join("lib.rs"), "pub fn lib() {}").unwrap();
    files_panel(harness.state_mut()).picker.fake_answer = Some(project.clone());
    click(&mut harness, "Upload folder…");
    wait_for(&mut harness, "merged upload", |_| {
        std::fs::read_to_string(root.join("project").join("src").join("lib.rs"))
            .is_ok_and(|t| t == "pub fn lib() {}")
    });
}

#[test]
fn key_generator_saves_a_key_pair() {
    let f = Fixture::new();
    let mut harness = harness(f.app(Config::default()));
    open_menu_item(&mut harness, "Tools", "Key generator…");
    let path = f.dir.path().join("keys").join("id_test");
    let Some(Dialog::KeyGenerator(generator)) = &mut harness.state_mut().dialog else {
        panic!("key generator not open");
    };
    generator.path.clear();
    generator.comment.clear();
    type_into(&mut harness, "Save as", &path.display().to_string());
    type_into(&mut harness, "Comment", "made by the test");
    type_into(&mut harness, "Passphrase", "secret");
    type_into(&mut harness, "Repeat passphrase", "different");
    harness.get_by_label("Generate").click();
    settle(&mut harness);
    harness.get_by_label("The passphrases do not match.");

    let Some(Dialog::KeyGenerator(generator)) = &mut harness.state_mut().dialog else {
        unreachable!()
    };
    generator.confirm = "secret".into();
    harness.get_by_label("Generate").click();
    settle(&mut harness);
    harness.get_by_label("Copy public key");
    let private = std::fs::read_to_string(&path).unwrap();
    let key = rooste_keys::load(&private, Some("secret")).unwrap();
    let public = std::fs::read_to_string(path.with_file_name("id_test.pub")).unwrap();
    assert!(public.starts_with("ssh-ed25519 ") && public.trim_end().ends_with("made by the test"));
    assert_eq!(
        rooste_keys::PublicKey::from_openssh(public.trim()).unwrap(),
        *key.public_key()
    );
}

#[test]
fn key_converter_turns_ppk_into_openssh() {
    let f = Fixture::new();
    let mut harness = harness(f.app(Config::default()));
    open_menu_item(&mut harness, "Tools", "Convert key (PuTTY / OpenSSH)…");
    let input = rooste_testkit::key_path("ppk").join("v3-ed25519-encrypted.ppk");
    let output = f.dir.path().join("converted");
    type_into(&mut harness, "Key file", &input.display().to_string());
    type_into(
        &mut harness,
        "Passphrase",
        rooste_testkit::TEST_KEY_PASSPHRASE,
    );
    let Some(Dialog::KeyConverter(converter)) = &mut harness.state_mut().dialog else {
        panic!("converter not open");
    };
    converter.output = output.display().to_string();
    harness.get_by_label("Convert").click();
    settle(&mut harness);
    harness.get_by_label(&format!("Saved an OpenSSH key to {}.", output.display()));
    let text = std::fs::read_to_string(&output).unwrap();
    assert!(text.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
    rooste_keys::load(&text, None).unwrap();
}

#[test]
fn builtin_agent_logs_into_sessions() {
    let f = Fixture::with_server();
    let mut harness = harness(f.app(Config {
        profiles: vec![Profile {
            use_agent: true,
            ..f.profile("agent login")
        }],
        ..Config::default()
    }));
    open_menu_item(&mut harness, "Tools", "SSH agent…");
    harness.get_by_label("Start agent").click();
    settle(&mut harness);
    assert!(harness.state().builtin_agent.is_some());
    type_into(
        &mut harness,
        "Key file",
        &rooste_testkit::key_path("id_ed25519").display().to_string(),
    );
    harness.get_by_label("Load key").click();
    settle(&mut harness);
    harness
        .get_by_label("Use this agent for Rooste SSH sessions")
        .click();
    settle(&mut harness);
    let Some(Dialog::Agent(window)) = &harness.state().dialog else {
        panic!("agent window closed");
    };
    assert_eq!(window.keys.len(), 1, "{:?}", window.error);
    harness.get_by_label("Close").click();
    settle(&mut harness);
    assert!(harness.state().config.settings.use_builtin_agent);

    // No password is saved, so only the agent can log in.
    context_menu(&mut harness, "agent login", "Connect");
    wait_for(&mut harness, "connected via the built-in agent", |app| {
        connected_count(app) == 1
    });
    assert!(first_tab(harness.state()).unwrap().prompt.is_none());
}

// ---- M5b: telnet, raw TCP and serial ----------------------------------------

fn plain_profile(name: &str, protocol: rooste_core::Protocol, port: u16) -> Profile {
    Profile {
        name: name.into(),
        protocol,
        host: "127.0.0.1".into(),
        port: Some(port),
        ..Profile::default()
    }
}

#[test]
fn telnet_session_reports_window_size() {
    let f = Fixture::new();
    let server = f.net.block_on(rooste_testkit::TelnetServer::start());
    let profile = plain_profile("switch", rooste_core::Protocol::Telnet, server.addr.port());
    let mut harness = harness(f.app(Config {
        profiles: vec![profile],
        ..Config::default()
    }));
    context_menu(&mut harness, "switch", "Connect");
    wait_for(&mut harness, "telnet prompt", |app| {
        connected_count(app) == 1 && screen_text(app).contains("Welcome")
    });
    let (cols, rows) = first_tab(harness.state()).unwrap().terminal.size();
    harness.event(egui::Event::Text("size".into()));
    harness.key_press(egui::Key::Enter);
    let expected = format!("{cols}x{rows}");
    wait_for(&mut harness, "size via NAWS", |app| {
        screen_text(app).contains(&expected)
    });
    harness.event(egui::Event::Text("ttype".into()));
    harness.key_press(egui::Key::Enter);
    wait_for(&mut harness, "terminal type", |app| {
        screen_text(app).contains("xterm-256color")
    });

    // File transfer is SSH-only.
    open_menu_item(&mut harness, "Tab", "Files (SFTP)");
    wait_for(&mut harness, "files refused", |app| {
        first_tab(app)
            .and_then(|t| t.files.as_ref())
            .is_some_and(|f| f.error.as_deref() == Some("File transfer needs an SSH connection."))
    });
}

#[test]
fn raw_tcp_session_echoes() {
    let f = Fixture::new();
    let echo = f.net.block_on(rooste_testkit::EchoServer::start());
    let profile = plain_profile("raw echo", rooste_core::Protocol::Raw, echo.addr.port());
    let mut harness = harness(f.app(Config {
        profiles: vec![profile],
        ..Config::default()
    }));
    context_menu(&mut harness, "raw echo", "Connect");
    wait_for(&mut harness, "connected", |app| connected_count(app) == 1);
    harness.event(egui::Event::Text("ping over raw".into()));
    wait_for(&mut harness, "echo", |app| {
        screen_text(app).contains("ping over raw")
    });
}

#[test]
fn serial_profile_editor() {
    let f = Fixture::new();
    let mut harness = harness(f.app(Config::default()));
    click(&mut harness, "➕");
    let Some(Dialog::EditProfile(editor)) = &mut harness.state_mut().dialog else {
        panic!("editor not open");
    };
    editor.profile.protocol = rooste_core::Protocol::Serial;
    editor.profile.name = "console".into();
    settle(&mut harness);
    assert!(
        harness.query_by_label("Jump host").is_none(),
        "SSH fields hidden"
    );
    harness.get_by_label("Speed (baud)");
    click(&mut harness, "Save");
    harness.get_by_label("Enter a serial port, such as COM3 or /dev/ttyUSB0.");
    type_into(&mut harness, "Serial port", "/dev/ttyUSB0");
    click(&mut harness, "Even");
    click(&mut harness, "RTS/CTS");
    click(&mut harness, "Save");
    let saved = f.saved_config();
    let p = &saved.profiles[0];
    assert_eq!(p.host, "/dev/ttyUSB0");
    assert_eq!(p.serial.describe(), "9600 8E1");
    assert_eq!(p.serial.flow_control, rooste_core::FlowControl::RtsCts);
}

#[test]
fn missing_serial_port_fails_clearly() {
    let f = Fixture::new();
    let port = if cfg!(windows) {
        "COM250"
    } else {
        "/dev/rooste-no-such-port"
    };
    let profile = Profile {
        name: "nowhere".into(),
        protocol: rooste_core::Protocol::Serial,
        host: port.into(),
        ..Profile::default()
    };
    let mut harness = harness(f.app(Config {
        profiles: vec![profile],
        ..Config::default()
    }));
    context_menu(&mut harness, "nowhere", "Connect");
    wait_for(
        &mut harness,
        "failure",
        |app| matches!(first_tab(app).map(|t| &t.state), Some(TabState::Failed(m)) if m.contains("Could not open the serial port") && m.contains(port)),
    );
}

/// Icons must come from the bundled fonts: a symbol that only some system
/// fonts have shows up as an empty box elsewhere.
#[test]
fn icons_are_in_the_bundled_fonts() {
    let mut harness = Harness::new_ui(|_ui| {});
    harness.step();
    let font = egui::FontId::proportional(14.0);
    for icon in "×⏷⏺🔘⬆⬇⟳➕−📁📄⚠📢✔✖".chars() {
        let found = harness.ctx.fonts_mut(|fonts| fonts.has_glyph(&font, icon));
        assert!(found, "{icon} (U+{:04X}) has no glyph", icon as u32);
    }
}
