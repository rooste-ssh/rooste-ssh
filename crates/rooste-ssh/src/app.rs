//! The application window: menu, session manager, tabs and dialogs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use egui::{RichText, Ui};
use egui_tiles::{Container, LinearDir, SimplificationOptions, Tile, TileId, Tree, UiResponse};
use rooste_core::import::{putty, ssh_config};
use rooste_core::{Config, ConfigLocation, Profile, Protocol, SecretStore, Settings, Theme};
use rooste_protocols::ssh::AgentSource;
use rooste_tmux::{
    LayoutNode, TmuxClientAction, TmuxNotification, TmuxPaneId, TmuxSessionId, TmuxWindowId,
    TmuxWindowSnapshot,
};

use crate::dialogs::{self, Decision, ProfileEditor, SettingsEditor};
use crate::manager::{self, ManagerAction, ManagerState};
use crate::palette::{self as command_palette, Command};
use crate::picker::{PickId, PickKind, Picker, path_row};
use crate::session::{ConnectRequest, SessionCommand};
use crate::tab::{SessionTab, TabAction, TmuxRole};
use crate::theme;
use crate::tools::{AgentAction, AgentWindow, KeyConverter, KeyGenerator};

pub use crate::tab::TabState;

/// Fields of the quick-connect form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectForm {
    pub host: String,
    pub port: String,
    pub user: String,
    pub password: String,
    pub key_file: String,
    pub use_agent: bool,
    pub allow_legacy: bool,
}

impl Default for ConnectForm {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: "22".into(),
            user: std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_default(),
            password: String::new(),
            key_file: String::new(),
            use_agent: true,
            allow_legacy: false,
        }
    }
}

impl ConnectForm {
    /// Validate the form into a request, or explain what is missing.
    pub fn request(&self) -> Result<ConnectRequest, String> {
        let host = self.host.trim();
        if host.is_empty() {
            return Err("Enter a host name.".into());
        }
        let port = self
            .port
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|&p| p != 0)
            .ok_or("Port must be a number between 1 and 65535.")?;
        let user = self.user.trim();
        if user.is_empty() {
            return Err("Enter a user name.".into());
        }
        let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_owned());
        Ok(ConnectRequest {
            host: host.to_owned(),
            port,
            user: user.to_owned(),
            password: non_empty(&self.password),
            key_file: non_empty(self.key_file.trim()).map(|p| rooste_core::expand_tilde(&p)),
            key_passphrase: None,
            use_agent: self.use_agent,
            allow_legacy: self.allow_legacy,
            known_hosts: None,
            cols: 80,
            rows: 24,
            prompt_for_password: true,
            proxy_jump: None,
            forward_agent: false,
            forward_x11: false,
            agent: AgentSource::Default,
            tunnels: Vec::new(),
            protocol: Protocol::Ssh,
            serial: Default::default(),
            tmux_command: None,
            tmux_scrollback_capture: 1_000,
            tmux_pause_after_secs: 5,
        })
    }
}

/// An open dialog.
#[derive(Debug)]
pub enum Dialog {
    EditProfile(Box<ProfileEditor>),
    Settings(SettingsEditor),
    DeleteProfile(String),
    CloseTab(TileId),
    QuickConnect,
    Palette { query: String },
    KeyGenerator(Box<KeyGenerator>),
    KeyConverter(Box<KeyConverter>),
    Agent(Box<AgentWindow>),
}

/// A message in the status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub error: bool,
    /// A web link shown after the text.
    pub link: Option<(String, String)>,
}

/// Where imports read from; overridable for tests.
#[derive(Debug, Clone, Default)]
pub struct ImportSources {
    pub putty_dir: Option<PathBuf>,
    pub ssh_config: Option<PathBuf>,
}

struct TabBehavior {
    close_requests: Vec<TileId>,
    /// Picked from a tab's right-click menu.
    menu_actions: Vec<(TileId, TabMenuAction)>,
    /// Where scripts are kept, for the "Run script" menu.
    scripts_dir: Option<PathBuf>,
    /// The selected tab being drawn and whether its terminal has keyboard
    /// focus, noted while choosing its text color (egui_tiles tells only that
    /// hook) for its accent line.
    selected_tab: std::cell::Cell<Option<(TileId, bool)>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TabMenuAction {
    Duplicate,
    SplitRight,
    SplitDown,
    Files,
    Log,
    Record,
    RunScript(String),
    StopScript,
    OpenScriptsFolder,
    Reconnect,
    TmuxNewWindow,
    TmuxDetach,
    TmuxSyncPanes,
    Close,
}

impl egui_tiles::Behavior<SessionTab> for TabBehavior {
    fn pane_ui(&mut self, ui: &mut Ui, _tile_id: TileId, tab: &mut SessionTab) -> UiResponse {
        tab.ui(ui);
        UiResponse::None
    }

    fn tab_title_for_pane(&mut self, tab: &SessionTab) -> egui::WidgetText {
        let mut job = egui::text::LayoutJob::default();
        let font = egui::FontId::proportional(14.0);
        if tab.has_ended() {
            let red = egui::TextFormat::simple(font.clone(), egui::Color32::from_rgb(220, 80, 80));
            job.append("⏺ ", 0.0, red);
        }
        job.append(
            &tab.title(),
            0.0,
            egui::TextFormat::simple(font, egui::Color32::PLACEHOLDER),
        );
        job.into()
    }

    fn is_tab_closable(&self, _tiles: &egui_tiles::Tiles<SessionTab>, _tile_id: TileId) -> bool {
        true
    }

    fn on_tab_close(
        &mut self,
        _tiles: &mut egui_tiles::Tiles<SessionTab>,
        tile_id: TileId,
    ) -> bool {
        // Closing is decided by the app (it may ask first).
        self.close_requests.push(tile_id);
        false
    }

    fn tab_bar_height(&self, _style: &egui::Style) -> f32 {
        30.0
    }

    fn tab_title_spacing(&self, _visuals: &egui::Visuals) -> f32 {
        12.0
    }

    /// Unselected tabs get a background between the bar's and the selected
    /// tab's, and an outline, so they still look like tabs.
    fn tab_bg_color(
        &self,
        visuals: &egui::Visuals,
        _tiles: &egui_tiles::Tiles<SessionTab>,
        _tile_id: TileId,
        state: &egui_tiles::TabState,
    ) -> egui::Color32 {
        if state.active {
            visuals.panel_fill
        } else {
            self.tab_bar_color(visuals)
                .lerp_to_gamma(visuals.panel_fill, 0.5)
        }
    }

    fn tab_outline_stroke(
        &self,
        visuals: &egui::Visuals,
        _tiles: &egui_tiles::Tiles<SessionTab>,
        _tile_id: TileId,
        state: &egui_tiles::TabState,
    ) -> egui::Stroke {
        if state.active {
            egui::Stroke::new(1.0, visuals.widgets.active.bg_fill)
        } else {
            egui::Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color)
        }
    }

    fn tab_text_color(
        &self,
        visuals: &egui::Visuals,
        tiles: &egui_tiles::Tiles<SessionTab>,
        tile_id: TileId,
        state: &egui_tiles::TabState,
    ) -> egui::Color32 {
        if state.active {
            let focused = matches!(tiles.get(tile_id), Some(Tile::Pane(tab)) if tab.view.has_focus);
            self.selected_tab.set(Some((tile_id, focused)));
            visuals.strong_text_color()
        } else {
            visuals.weak_text_color()
        }
    }

    fn tab_ui(
        &mut self,
        tiles: &mut egui_tiles::Tiles<SessionTab>,
        ui: &mut Ui,
        id: egui::Id,
        tile_id: TileId,
        state: &egui_tiles::TabState,
    ) -> egui::Response {
        let text = self.tab_title_for_tile(tiles, tile_id);
        let close_btn_size = egui::Vec2::splat(self.close_button_outer_size());
        let close_btn_left_padding = 4.0;
        let font_id = egui::TextStyle::Button.resolve(ui.style());
        let galley = text.into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, font_id);

        let x_margin = self.tab_title_spacing(ui.visuals());

        let button_width = galley.size().x
            + 2.0 * x_margin
            + f32::from(state.closable) * (close_btn_left_padding + close_btn_size.x);
        let (_, tab_rect) = ui.allocate_space(egui::vec2(button_width, ui.available_height()));

        let draggable = self.is_tile_draggable(tiles, tile_id);
        let sense = if draggable {
            egui::Sense::click_and_drag()
        } else {
            egui::Sense::click()
        };
        let tab_response = ui.interact(tab_rect, id, sense);
        let tab_response = if draggable {
            tab_response.on_hover_cursor(self.tab_hover_cursor_icon())
        } else {
            tab_response
        };

        tab_response.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::Role::Button,
                ui.is_enabled(),
                state.active,
                galley.text(),
            )
        });

        if ui.is_rect_visible(tab_rect) && !state.is_being_dragged {
            let bg_color = self.tab_bg_color(ui.visuals(), tiles, tile_id, state);
            let stroke = self.tab_outline_stroke(ui.visuals(), tiles, tile_id, state);
            ui.painter().rect(
                tab_rect.shrink(0.5),
                0.0,
                bg_color,
                stroke,
                egui::StrokeKind::Inside,
            );

            if state.active {
                ui.painter().hline(
                    tab_rect.x_range(),
                    tab_rect.bottom(),
                    egui::Stroke::new(stroke.width + 1.0, bg_color),
                );
            }

            let text_color = self.tab_text_color(ui.visuals(), tiles, tile_id, state);
            let text_position = egui::Align2::LEFT_CENTER
                .align_size_within_rect(galley.size(), tab_rect.shrink(x_margin))
                .min;

            ui.painter().galley(text_position, galley, text_color);

            if state.closable {
                let close_btn_rect = egui::Align2::RIGHT_CENTER
                    .align_size_within_rect(close_btn_size, tab_rect.shrink(x_margin));

                let close_btn_id = ui.auto_id_with("tab_close_btn");
                let close_btn_response = ui
                    .interact(close_btn_rect, close_btn_id, egui::Sense::click_and_drag())
                    .on_hover_cursor(egui::CursorIcon::Default);

                close_btn_response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), "Close")
                });

                let visuals = ui.style().interact(&close_btn_response);
                let rect = close_btn_rect
                    .shrink(self.close_button_inner_margin())
                    .expand(visuals.expansion);
                let stroke = visuals.fg_stroke;

                ui.painter()
                    .line_segment([rect.left_top(), rect.right_bottom()], stroke);
                ui.painter()
                    .line_segment([rect.right_top(), rect.left_bottom()], stroke);

                if (close_btn_response.clicked()
                    || tab_response.clicked_by(egui::PointerButton::Middle))
                    && self.on_tab_close(tiles, tile_id)
                {
                    tiles.remove(tile_id);
                }
            }
        }

        let resp = self.on_tab_button(tiles, tile_id, tab_response);

        // Beside the tmux gateway tab (main session), render an emoji "➕" tag to create a new tmux window.
        let is_gateway = matches!(
            tiles.get(tile_id),
            Some(Tile::Pane(tab)) if matches!(tab.tmux_role, TmuxRole::Gateway { .. })
        );
        if is_gateway {
            ui.add_space(2.0);
            let plus_width = 28.0;
            let (plus_rect, plus_response) = ui.allocate_exact_size(
                egui::vec2(plus_width, ui.available_height()),
                egui::Sense::click(),
            );
            if ui.is_rect_visible(plus_rect) {
                let visuals = ui.visuals();
                let hovered = plus_response.hovered();
                let bg_color = if hovered {
                    visuals.widgets.hovered.bg_fill
                } else {
                    self.tab_bar_color(visuals)
                        .lerp_to_gamma(visuals.panel_fill, 0.5)
                };
                let stroke = if hovered {
                    egui::Stroke::new(1.0, visuals.widgets.hovered.bg_stroke.color)
                } else {
                    egui::Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color)
                };
                ui.painter().rect(
                    plus_rect.shrink(0.5),
                    0.0,
                    bg_color,
                    stroke,
                    egui::StrokeKind::Inside,
                );
                let font_id = egui::TextStyle::Button.resolve(ui.style());
                let text_color = if hovered {
                    visuals.strong_text_color()
                } else {
                    visuals.text_color()
                };
                let plus_galley = ui
                    .painter()
                    .layout_no_wrap("➕".to_owned(), font_id, text_color);
                let text_pos = egui::Align2::CENTER_CENTER
                    .align_size_within_rect(plus_galley.size(), plus_rect)
                    .min;
                ui.painter().galley(text_pos, plus_galley, text_color);
            }
            let plus_response = plus_response
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text("New tmux window");
            if plus_response.clicked() {
                self.menu_actions
                    .push((tile_id, TabMenuAction::TmuxNewWindow));
            }
        }

        resp
    }

    fn on_tab_button(
        &mut self,
        tiles: &mut egui_tiles::Tiles<SessionTab>,
        tile_id: TileId,
        response: egui::Response,
    ) -> egui::Response {
        if response.clicked()
            && let Some(Tile::Pane(tab)) = tiles.get_mut(tile_id)
        {
            tab.view.focus_requested = true;
        }
        let Some(Tile::Pane(tab)) = tiles.get(tile_id) else {
            return response;
        };
        // A line along the top of the selected tab in each group: in the
        // accent color where the keyboard input goes, grey in other groups.
        if let Some((selected, focused)) = self.selected_tab.take()
            && selected == tile_id
        {
            let visuals = &response.ctx.global_style().visuals;
            let is_gateway = matches!(tab.tmux_role, TmuxRole::Gateway { .. });
            let accent = if focused {
                if is_gateway {
                    egui::Color32::from_rgb(245, 130, 32)
                } else {
                    visuals.hyperlink_color
                }
            } else {
                egui::Color32::from_gray(if visuals.dark_mode { 110 } else { 160 })
            };
            // Matches the outline, which egui_tiles draws half a point inside.
            let rect = response.rect.shrink(0.5);
            response
                .ctx
                .layer_painter(response.layer_id)
                .with_clip_rect(response.interact_rect)
                .rect_filled(
                    egui::Rect::from_min_max(
                        rect.left_top(),
                        egui::pos2(rect.right(), rect.top() + 3.0),
                    ),
                    0.0,
                    accent,
                );
        }
        let ended = tab.has_ended();
        let logging = tab.log.is_some();
        let recording = tab.recording.is_some();
        let running_script = tab.script.is_some();
        let is_tmux = !matches!(tab.tmux_role, TmuxRole::None);
        let is_tmux_pane = matches!(tab.tmux_role, TmuxRole::Pane { .. });
        let scripts_dir = self.scripts_dir.clone();
        response.context_menu(|ui| {
            let mut item = |ui: &mut Ui, label: &str, action: TabMenuAction| {
                if ui.button(label).clicked() {
                    self.menu_actions.push((tile_id, action));
                    ui.close();
                }
            };
            item(ui, "Duplicate tab", TabMenuAction::Duplicate);
            item(ui, "Split right", TabMenuAction::SplitRight);
            item(ui, "Split down", TabMenuAction::SplitDown);
            item(ui, "Files (SFTP)", TabMenuAction::Files);
            let log_label = if logging {
                "Stop logging"
            } else {
                "Start logging…"
            };
            item(ui, log_label, TabMenuAction::Log);
            let record_label = if recording {
                "Stop recording"
            } else {
                "Record session…"
            };
            item(ui, record_label, TabMenuAction::Record);
            if running_script {
                item(ui, "Stop script", TabMenuAction::StopScript);
            } else {
                ui.menu_button("Run script", |ui| {
                    let scripts = scripts_dir
                        .as_deref()
                        .map(crate::scripting::list_scripts)
                        .unwrap_or_default();
                    if scripts.is_empty() {
                        ui.weak("No scripts yet");
                    }
                    for name in scripts {
                        item(ui, &name, TabMenuAction::RunScript(name.clone()));
                    }
                    ui.separator();
                    item(ui, "Open scripts folder", TabMenuAction::OpenScriptsFolder);
                });
            }
            if is_tmux_pane {
                ui.separator();
                item(ui, "tmux: Synchronize panes", TabMenuAction::TmuxSyncPanes);
            }
            if is_tmux {
                item(ui, "tmux: New window", TabMenuAction::TmuxNewWindow);
                item(ui, "tmux: Detach session", TabMenuAction::TmuxDetach);
            }
            if ended {
                item(ui, "Reconnect", TabMenuAction::Reconnect);
            }
            ui.separator();
            item(ui, "Close tab", TabMenuAction::Close);
        });
        response
    }

    fn simplification_options(&self) -> SimplificationOptions {
        SimplificationOptions {
            all_panes_must_have_tabs: true,
            ..SimplificationOptions::default()
        }
    }
}

/// The rooste-ssh application.
type UpdateReceiver = std::sync::mpsc::Receiver<Result<Option<crate::update::Release>, String>>;

pub struct RoosteApp {
    runtime: tokio::runtime::Runtime,
    pub config: Config,
    config_path: Option<PathBuf>,
    secrets: Arc<dyn SecretStore>,
    pub tree: Tree<SessionTab>,
    pub manager: ManagerState,
    pub dialog: Option<Dialog>,
    pub quick: ConnectForm,
    pub quick_error: Option<String>,
    pub notice: Option<Notice>,
    pub show_manager: bool,
    /// Input in one tab goes to all connected tabs.
    pub broadcast: bool,
    agent: AgentSource,
    /// The built-in SSH agent, once started.
    pub builtin_agent: Option<rooste_keys::Agent>,
    /// Result of the startup update check, when it finishes.
    /// A running update check, and whether the user asked for it.
    update_check: Option<(UpdateReceiver, bool)>,
    /// Where update checks ask; overridable for tests.
    update_api: String,
    /// A newer release to tell the user about, in a banner.
    pub update_banner: Option<crate::update::Release>,
    /// The UI context, once known, to wake the UI from background threads.
    ctx: Option<egui::Context>,
    /// Serial ports found when the profile editor opened.
    serial_ports: Vec<String>,
    /// Native file dialogs for the "Browse…" buttons.
    pub picker: Picker,
    /// The tab waiting for the "Start logging…" dialog.
    log_target: Option<TileId>,
    /// The tab waiting for the "Record session…" dialog.
    recording_target: Option<TileId>,
    known_hosts: Option<PathBuf>,
    imports: ImportSources,
    window_title: String,
    applied_theme: Option<Theme>,
    /// Each tab group's selected tab in the last frame, to move the keyboard
    /// focus along when it changes.
    selected_tabs: HashMap<TileId, TileId>,
}

impl std::fmt::Debug for RoosteApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoosteApp")
            .field(
                "tabs",
                &self.tabs().map(|t| (&t.name, &t.state)).collect::<Vec<_>>(),
            )
            .field("dialog", &self.dialog)
            .field("notice", &self.notice)
            .finish_non_exhaustive()
    }
}

impl RoosteApp {
    /// The app with the user's configuration and the OS credential store.
    pub fn new() -> Self {
        let location = ConfigLocation::detect();
        let (config, path, notice) = match Config::load(&location.path) {
            Ok(config) => (config, Some(location.path), None),
            Err(e) => (
                Config::default(),
                // Do not overwrite a file we could not read.
                None,
                Some(Notice {
                    text: format!("{e}; changes will not be saved"),
                    error: true,
                    link: None,
                }),
            ),
        };
        let mut app = Self::with_config(config, path, Arc::new(crate::secrets::KeyringStore));
        app.notice = notice;
        app
    }

    /// An app with the given configuration. `config_path` of `None` keeps
    /// changes in memory only.
    pub fn with_config(
        config: Config,
        config_path: Option<PathBuf>,
        secrets: Arc<dyn SecretStore>,
    ) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("rooste-net")
            .enable_all()
            .build()
            .expect("start tokio runtime");
        let picker = Picker::new(runtime.handle().clone());
        Self {
            runtime,
            config,
            config_path,
            secrets,
            tree: Tree::empty(egui::Id::unique("tabs")),
            manager: ManagerState::default(),
            dialog: None,
            quick: ConnectForm::default(),
            quick_error: None,
            notice: None,
            show_manager: true,
            broadcast: false,
            agent: AgentSource::Default,
            builtin_agent: None,
            update_check: None,
            update_api: crate::update::RELEASES_API.into(),
            update_banner: None,
            ctx: None,
            serial_ports: Vec::new(),
            picker,
            log_target: None,
            recording_target: None,
            known_hosts: None,
            imports: ImportSources::default(),
            window_title: String::new(),
            applied_theme: None,
            selected_tabs: HashMap::new(),
        }
    }

    /// Use a specific known_hosts file instead of `~/.ssh/known_hosts`.
    pub fn with_known_hosts(mut self, path: PathBuf) -> Self {
        self.known_hosts = Some(path);
        self
    }

    /// Check GitHub for a newer release in the background, if enabled.
    pub fn with_update_check(mut self) -> Self {
        if self.config.settings.check_updates {
            self.check_for_updates(false);
        }
        self
    }

    /// The UI context, so background work can wake the UI.
    pub fn with_context(mut self, ctx: egui::Context) -> Self {
        self.ctx = Some(ctx);
        self
    }

    /// Ask this URL instead of GitHub for the latest release (tests).
    pub fn with_update_api(mut self, url: impl Into<String>) -> Self {
        self.update_api = url.into();
        self
    }

    /// Look for a newer release in the background. A check the user asked
    /// for also reports "up to date" and errors, and ignores a skipped
    /// version.
    pub fn check_for_updates(&mut self, manual: bool) {
        let (tx, rx) = std::sync::mpsc::channel();
        let api = self.update_api.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let latest = crate::update::fetch_latest(
                &api,
                env!("CARGO_PKG_VERSION"),
                crate::update::Install::detect(),
            );
            let _ = tx.send(latest);
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
        });
        self.update_check = Some((rx, manual));
    }

    fn update_result(&mut self) {
        let Some((rx, manual)) = &self.update_check else {
            return;
        };
        let manual = *manual;
        let Ok(result) = rx.try_recv() else { return };
        self.update_check = None;
        match result {
            Ok(Some(release)) => {
                if manual || release.version != self.config.settings.skipped_version {
                    self.update_banner = Some(release);
                }
            }
            Ok(None) if manual => self.notify(
                format!(
                    "You have the latest version of Rooste SSH ({}).",
                    env!("CARGO_PKG_VERSION")
                ),
                false,
            ),
            Err(e) if manual => self.notify(format!("Could not check for updates: {e}"), true),
            _ => {}
        }
    }

    /// The "new version" banner below the menu bar.
    fn update_banner_ui(&mut self, ui: &mut Ui) {
        let Some(release) = self.update_banner.clone() else {
            return;
        };
        let fill = ui.visuals().faint_bg_color;
        egui::Panel::top("update-banner")
            .frame(
                egui::Frame::side_top_panel(&ui.ctx().global_style())
                    .fill(fill)
                    .inner_margin(egui::Margin::symmetric(10, 6)),
            )
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "Rooste SSH {} is available (you have {}).",
                            release.version,
                            env!("CARGO_PKG_VERSION")
                        ))
                        .strong(),
                    );
                    if ui.button("Release notes").clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(&release.url));
                    }
                    if ui.button("Download").clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(&release.download));
                    }
                    if ui
                        .button("Skip this version")
                        .on_hover_text("Don't tell me about this version again")
                        .clicked()
                    {
                        self.config.settings.skipped_version = release.version.clone();
                        self.save_config();
                        self.update_banner = None;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("×")
                            .on_hover_text("Hide until next start")
                            .clicked()
                        {
                            self.update_banner = None;
                        }
                    });
                });
            });
    }

    /// Use a specific SSH agent for login and forwarding.
    pub fn with_agent(mut self, agent: AgentSource) -> Self {
        self.agent = agent;
        self
    }

    pub fn with_import_sources(mut self, imports: ImportSources) -> Self {
        self.imports = imports;
        self
    }

    pub fn secrets(&self) -> &dyn SecretStore {
        &*self.secrets
    }

    /// All open tabs.
    pub fn tabs(&self) -> impl Iterator<Item = &SessionTab> {
        self.tree.tiles.tiles().filter_map(|tile| match tile {
            Tile::Pane(tab) => Some(tab),
            Tile::Container(_) => None,
        })
    }

    /// All open tabs, mutably.
    pub fn tabs_mut(&mut self) -> impl Iterator<Item = &mut SessionTab> {
        self.tree
            .tiles
            .iter_mut()
            .filter_map(|(_, tile)| match tile {
                Tile::Pane(tab) => Some(tab),
                Tile::Container(_) => None,
            })
    }

    fn tab_ids(&self) -> Vec<TileId> {
        self.tree
            .tiles
            .iter()
            .filter(|(_, tile)| matches!(tile, Tile::Pane(_)))
            .map(|(id, _)| *id)
            .collect()
    }

    fn tab(&self, id: TileId) -> Option<&SessionTab> {
        match self.tree.tiles.get(id) {
            Some(Tile::Pane(tab)) => Some(tab),
            _ => None,
        }
    }

    fn tab_mut(&mut self, id: TileId) -> Option<&mut SessionTab> {
        match self.tree.tiles.get_mut(id) {
            Some(Tile::Pane(tab)) => Some(tab),
            _ => None,
        }
    }

    fn find_tile_by_tab_id(&self, tab_id: u64) -> Option<TileId> {
        self.tree
            .tiles
            .iter()
            .find_map(|(tile_id, tile)| match tile {
                Tile::Pane(tab) if tab.id == tab_id => Some(*tile_id),
                _ => None,
            })
    }

    fn find_tmux_pane_tile(&self, gateway_tab_id: u64, pane_id: TmuxPaneId) -> Option<TileId> {
        self.tree
            .tiles
            .iter()
            .find_map(|(tile_id, tile)| match tile {
                Tile::Pane(tab)
                    if matches!(
                        tab.tmux_role,
                        TmuxRole::Pane {
                            gateway_tab_id: gw,
                            pane_id: pid,
                            ..
                        } if gw == gateway_tab_id && pid == pane_id
                    ) =>
                {
                    Some(*tile_id)
                }
                _ => None,
            })
    }

    fn has_active_tmux(&self) -> bool {
        self.tabs()
            .any(|tab| !matches!(tab.tmux_role, TmuxRole::None))
    }

    /// The first visible tab.
    fn active_tab_id(&self) -> Option<TileId> {
        self.tree
            .active_tiles()
            .into_iter()
            .find(|id| matches!(self.tree.tiles.get(*id), Some(Tile::Pane(_))))
    }

    fn save_config(&mut self) {
        if let Some(path) = &self.config_path
            && let Err(e) = self.config.save(path)
        {
            self.notice = Some(Notice {
                text: e.to_string(),
                error: true,
                link: None,
            });
        }
    }

    fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.notice = Some(Notice {
            text: text.into(),
            error,
            link: None,
        });
    }

    fn request_for(&self, profile: &Profile) -> ConnectRequest {
        let user = if profile.user.is_empty() {
            self.config.settings.default_user.clone()
        } else {
            profile.user.clone()
        };
        ConnectRequest {
            host: profile.host.clone(),
            port: profile.effective_port(),
            user,
            password: profile
                .save_password
                .then(|| self.secrets.get(&profile.id))
                .flatten(),
            key_file: profile
                .key_file
                .as_ref()
                .map(|p| rooste_core::expand_tilde(&p.to_string_lossy())),
            key_passphrase: None,
            use_agent: profile.use_agent,
            allow_legacy: profile.allow_legacy,
            known_hosts: self.known_hosts.clone(),
            cols: 80,
            rows: 24,
            prompt_for_password: true,
            proxy_jump: profile.proxy_jump.clone(),
            forward_agent: profile.forward_agent,
            forward_x11: profile.forward_x11,
            agent: self.session_agent(),
            tunnels: profile.tunnels.clone(),
            protocol: profile.protocol,
            serial: profile.serial,
            tmux_command: profile.tmux_attach_command(),
            tmux_scrollback_capture: self.config.settings.tmux_scrollback_capture,
            tmux_pause_after_secs: self.config.settings.tmux_pause_after_secs,
        }
    }

    /// Open a tab for a saved profile.
    pub fn connect_profile(&mut self, ctx: &egui::Context, profile_id: &str) {
        let Some(profile) = self.config.profile(profile_id).cloned() else {
            return;
        };
        let request = self.request_for(&profile);
        let mut tab = SessionTab::open(
            self.runtime.handle(),
            ctx,
            profile.display_name(),
            request,
            &self.config.settings,
        );
        tab.profile_id = Some(profile.id.clone());
        tab.save_password = profile.save_password;
        if !profile.on_connect_script.is_empty()
            && let Some(dir) = self.scripts_dir()
        {
            tab.on_connect_script = Some(dir.join(&profile.on_connect_script));
        }
        self.add_tab(tab);
    }

    fn quick_connect(&mut self, ctx: &egui::Context) -> bool {
        match self.quick.request() {
            Ok(mut request) => {
                request.known_hosts = self.known_hosts.clone();
                request.tmux_scrollback_capture = self.config.settings.tmux_scrollback_capture;
                request.tmux_pause_after_secs = self.config.settings.tmux_pause_after_secs;
                let name = format!("{}@{}", request.user, request.host);
                let tab = SessionTab::open(
                    self.runtime.handle(),
                    ctx,
                    name,
                    request,
                    &self.config.settings,
                );
                self.add_tab(tab);
                self.quick_error = None;
                self.quick.password.clear();
                true
            }
            Err(message) => {
                self.quick_error = Some(message);
                false
            }
        }
    }

    fn add_tab(&mut self, tab: SessionTab) {
        self.add_tab_beside(tab, self.active_tab_id());
    }

    /// Add a tab to the tab group of `beside` (or the root).
    fn add_tab_beside(&mut self, mut tab: SessionTab, beside: Option<TileId>) -> TileId {
        // A new tab takes the keyboard focus.
        tab.view.focus_requested = true;
        let id = self.tree.tiles.insert_pane(tab);
        let target = beside
            .and_then(|tile| self.tree.tiles.parent_of(tile))
            .or(self.tree.root);
        match target.and_then(|t| self.tree.tiles.get_mut(t)) {
            Some(Tile::Container(Container::Tabs(tabs))) => {
                tabs.add_child(id);
                tabs.set_active(id);
            }
            _ => {
                let children = self.tree.root.into_iter().chain([id]).collect();
                let tabs = self.tree.tiles.insert_tab_tile(children);
                if let Some(Tile::Container(Container::Tabs(t))) = self.tree.tiles.get_mut(tabs) {
                    t.set_active(id);
                }
                self.tree.root = Some(tabs);
            }
        }
        id
    }

    /// Reconcile a full `tmux -CC` topology snapshot into `egui_tiles::Tree<SessionTab>`.
    fn sync_tmux_topology(
        &mut self,
        ctx: &egui::Context,
        gateway_tab_id: u64,
        session_id: TmuxSessionId,
        session_name: String,
        windows: Vec<TmuxWindowSnapshot>,
    ) {
        let Some(gateway_tile) = self.find_tile_by_tab_id(gateway_tab_id) else {
            return;
        };
        let Some((sender, request, profile_id)) =
            self.tree.tiles.get_pane(&gateway_tile).and_then(|gw| {
                Some((
                    gw.session_sender()?,
                    gw.request.clone(),
                    gw.profile_id.clone(),
                ))
            })
        else {
            return;
        };

        let settings = self.config.settings.clone();
        let live_panes: Vec<TmuxPaneId> = windows
            .iter()
            .flat_map(|w| w.panes.iter().map(|p| p.pane_id))
            .collect();
        let dead_tiles: Vec<TileId> = self
            .tree
            .tiles
            .iter()
            .filter_map(|(tile_id, tile)| match tile {
                Tile::Pane(tab)
                    if matches!(
                        tab.tmux_role,
                        TmuxRole::Pane {
                            gateway_tab_id: gw,
                            pane_id,
                            ..
                        } if gw == gateway_tab_id && !live_panes.contains(&pane_id)
                    ) =>
                {
                    Some(*tile_id)
                }
                _ => None,
            })
            .collect();
        for tile_id in dead_tiles {
            self.remove_tab_locally(tile_id);
        }

        for win in windows {
            for pane in &win.panes {
                if let Some(tile_id) = self.find_tmux_pane_tile(gateway_tab_id, pane.pane_id) {
                    if let Some(tab) = self.tab_mut(tile_id) {
                        tab.name =
                            format!("[tmux:{}] {} ({})", session_name, win.name, pane.pane_id);
                        if !pane.title.is_empty() {
                            tab.remote_title = Some(pane.title.clone());
                        }
                        if let TmuxRole::Pane {
                            window_id,
                            window_name,
                            session_name: s_name,
                            sender: s_sender,
                            cwd,
                            remote_geom,
                            ..
                        } = &mut tab.tmux_role
                        {
                            *window_id = win.id;
                            *window_name = win.name.clone();
                            *s_name = session_name.clone();
                            *s_sender = sender.clone();
                            if !pane.cwd.is_empty() {
                                *cwd = pane.cwd.clone();
                            }
                            *remote_geom = (pane.cols.clamp(1, 1000), pane.rows.clamp(1, 1000));
                        }
                    }
                } else {
                    let tab = SessionTab::open_tmux_pane(
                        self.runtime.handle(),
                        ctx,
                        gateway_tab_id,
                        sender.clone(),
                        session_id,
                        &session_name,
                        win.id,
                        win.name.clone(),
                        pane.pane_id,
                        pane.cols,
                        pane.rows,
                        pane.title.clone(),
                        pane.cwd.clone(),
                        request.clone(),
                        profile_id.clone(),
                        &settings,
                    );
                    self.add_tab_beside(tab, Some(gateway_tile));
                }
            }
            if let Some(layout) = &win.layout {
                self.apply_tmux_window_layout(gateway_tab_id, win.id, layout);
            }
        }
    }

    /// Reorganize split containers in `self.tree` when a tmux window has multiple split panes.
    fn apply_tmux_window_layout(
        &mut self,
        gateway_tab_id: u64,
        window_id: TmuxWindowId,
        layout: &LayoutNode,
    ) {
        let panes = layout.panes();
        let dead_tiles: Vec<TileId> = self
            .tree
            .tiles
            .iter()
            .filter_map(|(tile_id, tile)| match tile {
                Tile::Pane(tab)
                    if matches!(
                        tab.tmux_role,
                        TmuxRole::Pane {
                            gateway_tab_id: gw,
                            window_id: wid,
                            pane_id,
                            ..
                        } if gw == gateway_tab_id && wid == window_id && !panes.iter().any(|(p, _)| *p == pane_id)
                    ) =>
                {
                    Some(*tile_id)
                }
                _ => None,
            })
            .collect();
        for tile_id in dead_tiles {
            self.remove_tab_locally(tile_id);
        }

        for (pane_id, geom) in &panes {
            if let Some(tile_id) = self.find_tmux_pane_tile(gateway_tab_id, *pane_id)
                && let Some(tab) = self.tab_mut(tile_id)
                && let TmuxRole::Pane { remote_geom, .. } = &mut tab.tmux_role
            {
                *remote_geom = (geom.cols.clamp(1, 1000), geom.rows.clamp(1, 1000));
            }
        }
    }

    fn handle_tmux_notification(&mut self, gateway_tab_id: u64, notification: TmuxNotification) {
        match notification {
            TmuxNotification::WindowClose { window } => {
                let to_remove: Vec<TileId> = self
                    .tree
                    .tiles
                    .iter()
                    .filter_map(|(tile_id, tile)| match tile {
                        Tile::Pane(tab)
                            if matches!(
                                tab.tmux_role,
                                TmuxRole::Pane {
                                    gateway_tab_id: gw,
                                    window_id: wid,
                                    ..
                                } if gw == gateway_tab_id && wid == window
                            ) =>
                        {
                            Some(*tile_id)
                        }
                        _ => None,
                    })
                    .collect();
                for tile_id in to_remove {
                    self.remove_tab_locally(tile_id);
                }
            }
            TmuxNotification::PaneExited { pane } => {
                if let Some(tile_id) = self.find_tmux_pane_tile(gateway_tab_id, pane) {
                    self.remove_tab_locally(tile_id);
                }
            }
            TmuxNotification::WindowRenamed { window, name } => {
                let ids = self.tab_ids();
                for tile_id in ids {
                    if let Some(tab) = self.tab_mut(tile_id)
                        && let TmuxRole::Pane {
                            gateway_tab_id: gw,
                            window_id: wid,
                            pane_id,
                            window_name,
                            session_name,
                            ..
                        } = &mut tab.tmux_role
                        && *gw == gateway_tab_id
                        && *wid == window
                    {
                        *window_name = name.clone();
                        tab.name = format!("[tmux:{session_name}] {} ({})", name, pane_id);
                    }
                }
            }
            TmuxNotification::LayoutChange { window, layout, .. } => {
                self.apply_tmux_window_layout(gateway_tab_id, window, &layout);
            }
            TmuxNotification::Pause { pane } => {
                if let Some(tile_id) = self.find_tmux_pane_tile(gateway_tab_id, pane)
                    && let Some(tab) = self.tab_mut(tile_id)
                    && let TmuxRole::Pane { paused, .. } = &mut tab.tmux_role
                {
                    *paused = true;
                }
            }
            TmuxNotification::Continue { pane } => {
                if let Some(tile_id) = self.find_tmux_pane_tile(gateway_tab_id, pane)
                    && let Some(tab) = self.tab_mut(tile_id)
                    && let TmuxRole::Pane { paused, .. } = &mut tab.tmux_role
                {
                    *paused = false;
                }
            }
            _ => {}
        }
    }

    fn handle_tmux_detached(&mut self, gateway_tab_id: u64, reason: Option<String>) {
        let to_remove: Vec<TileId> = self
            .tree
            .tiles
            .iter()
            .filter_map(|(tile_id, tile)| match tile {
                Tile::Pane(tab)
                    if matches!(
                        tab.tmux_role,
                        TmuxRole::Pane {
                            gateway_tab_id: gw,
                            ..
                        } if gw == gateway_tab_id
                    ) =>
                {
                    Some(*tile_id)
                }
                _ => None,
            })
            .collect();
        for tile_id in to_remove {
            self.remove_tab_locally(tile_id);
        }
        let msg = match reason {
            Some(r) if !r.is_empty() => format!("Detached from tmux session ({r})."),
            _ => "Detached from tmux session.".to_owned(),
        };
        self.notify(msg, false);
    }

    /// When a tab group switches to another tab (egui_tiles does that on
    /// mouse press, before a click), the new tab takes the keyboard focus if
    /// the old one had it.
    fn follow_tab_switches(&mut self) {
        let selected: HashMap<TileId, TileId> = self
            .tree
            .tiles
            .iter()
            .filter_map(|(&group, tile)| match tile {
                Tile::Container(Container::Tabs(tabs)) => tabs.active.map(|tab| (group, tab)),
                _ => None,
            })
            .collect();
        for (group, &tab) in &selected {
            if let Some(&old) = self.selected_tabs.get(group)
                && old != tab
                && self
                    .tree
                    .tiles
                    .get_pane(&old)
                    .is_some_and(|t| t.view.has_focus)
                && let Some(new) = self.tab_mut(tab)
            {
                new.view.focus_requested = true;
            }
        }
        self.selected_tabs = selected;
    }

    fn request_close(&mut self, id: TileId) {
        let confirm = self.config.settings.confirm_close;
        match self.tab_mut(id) {
            Some(tab) if confirm && tab.is_active() => self.dialog = Some(Dialog::CloseTab(id)),
            Some(_) => self.close_tab(id),
            None => {}
        }
    }

    fn remove_tab_locally(&mut self, id: TileId) {
        if let Some(tab) = self.tab_mut(id) {
            tab.close_locally();
        }
        self.tree.remove_recursively(id);
    }

    fn close_tab(&mut self, id: TileId) {
        let gateway_to_clean =
            self.tree.tiles.get_pane(&id).and_then(|tab| {
                matches!(tab.tmux_role, TmuxRole::Gateway { .. }).then_some(tab.id)
            });
        if let Some(tab) = self.tab_mut(id) {
            tab.close();
        }
        self.tree.remove_recursively(id);
        if let Some(gw_id) = gateway_to_clean {
            self.handle_tmux_detached(gw_id, None);
        }
    }

    /// The agent offered to sessions.
    fn session_agent(&self) -> AgentSource {
        match &self.builtin_agent {
            Some(agent) if self.config.settings.use_builtin_agent => {
                AgentSource::Path(agent.path().to_owned())
            }
            _ => self.agent.clone(),
        }
    }

    fn agent_action(&mut self, window: &mut AgentWindow, action: AgentAction) {
        window.error = None;
        let result: Result<(), String> = match action {
            AgentAction::Start => self
                .runtime
                .block_on(rooste_keys::Agent::start(None))
                .map(|agent| self.builtin_agent = Some(agent))
                .map_err(|e| format!("Could not start the agent: {e}")),
            AgentAction::Stop => {
                self.builtin_agent = None;
                window.keys.clear();
                Ok(())
            }
            AgentAction::Load => (|| {
                let agent = self
                    .builtin_agent
                    .as_ref()
                    .ok_or("The agent is not running.")?;
                let path = rooste_core::expand_tilde(window.key_file.trim());
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                let passphrase =
                    (!window.passphrase.is_empty()).then_some(window.passphrase.as_str());
                let key = rooste_keys::load(&text, passphrase).map_err(|e| e.to_string())?;
                self.runtime
                    .block_on(agent.add(&key))
                    .map_err(|e| e.to_string())?;
                window.key_file.clear();
                window.passphrase.clear();
                Ok(())
            })(),
            AgentAction::Remove(fingerprint) => (|| {
                let agent = self
                    .builtin_agent
                    .as_ref()
                    .ok_or("The agent is not running.")?;
                let key = window
                    .keys
                    .iter()
                    .find(|k| rooste_keys::fingerprint(k) == fingerprint)
                    .ok_or("No such key.")?;
                self.runtime
                    .block_on(agent.remove(key))
                    .map_err(|e| e.to_string())
            })(),
            AgentAction::Refresh => Ok(()),
        };
        if let Err(e) = result {
            window.error = Some(e);
        }
        if let Some(agent) = &self.builtin_agent {
            match self.runtime.block_on(agent.keys()) {
                Ok(keys) => window.keys = keys,
                Err(e) => window.error = Some(e.to_string()),
            }
        }
    }

    /// A new tab connecting like the active one.
    pub fn duplicate_active(&mut self) {
        if let Some(id) = self.active_tab_id() {
            self.duplicate_tab(id);
        }
    }

    /// A new tab connecting like `id`, in the same tab group (or a new tmux window).
    fn duplicate_tab(&mut self, id: TileId) {
        if let Some(Tile::Pane(tab)) = self.tree.tiles.get(id)
            && let TmuxRole::Pane { sender, cwd, .. } = &tab.tmux_role
        {
            sender.send(SessionCommand::TmuxAction(TmuxClientAction::NewWindow {
                cwd: (!cwd.is_empty()).then(|| cwd.clone()),
            }));
            return;
        }
        let settings = self.config.settings.clone();
        if let Some(tab) = self
            .tree
            .tiles
            .get_pane(&id)
            .map(|tab| tab.duplicate(&settings))
        {
            self.add_tab_beside(tab, Some(id));
        }
    }

    /// Open a copy of the active tab beside it.
    pub fn split_active(&mut self, dir: LinearDir) {
        if let Some(id) = self.active_tab_id() {
            self.split_tab(id, dir);
        }
    }

    /// Open a copy of tab `active` beside its tab group (or split the remote tmux pane).
    fn split_tab(&mut self, active: TileId, dir: LinearDir) {
        if let Some(Tile::Pane(tab)) = self.tree.tiles.get(active)
            && let TmuxRole::Pane {
                sender,
                pane_id,
                cwd,
                ..
            } = &tab.tmux_role
        {
            let cwd_opt = (!cwd.is_empty()).then(|| cwd.clone());
            let action = match dir {
                LinearDir::Horizontal => TmuxClientAction::SplitHorizontal {
                    pane: *pane_id,
                    cwd: cwd_opt,
                },
                LinearDir::Vertical => TmuxClientAction::SplitVertical {
                    pane: *pane_id,
                    cwd: cwd_opt,
                },
            };
            sender.send(SessionCommand::TmuxAction(action));
            return;
        }

        let settings = self.config.settings.clone();
        let Some(mut tab) = self
            .tree
            .tiles
            .get_pane(&active)
            .map(|t| t.duplicate(&settings))
        else {
            return;
        };
        tab.view.focus_requested = true;
        // Split the tab group holding the active tab.
        let group = self.tree.tiles.parent_of(active).unwrap_or(active);
        let pane = self.tree.tiles.insert_pane(tab);
        let new_group = self.tree.tiles.insert_tab_tile(vec![pane]);
        let split = match dir {
            LinearDir::Horizontal => self
                .tree
                .tiles
                .insert_horizontal_tile(vec![group, new_group]),
            LinearDir::Vertical => self.tree.tiles.insert_vertical_tile(vec![group, new_group]),
        };
        match self.tree.tiles.parent_of(group).filter(|&p| p != split) {
            Some(parent) => {
                if let Some(Tile::Container(container)) = self.tree.tiles.get_mut(parent) {
                    replace_child(container, group, split);
                }
            }
            None => self.tree.root = Some(split),
        }
    }

    fn tab_menu_action(&mut self, id: TileId, action: TabMenuAction) {
        match action {
            TabMenuAction::Duplicate => self.duplicate_tab(id),
            TabMenuAction::SplitRight => self.split_tab(id, LinearDir::Horizontal),
            TabMenuAction::SplitDown => self.split_tab(id, LinearDir::Vertical),
            TabMenuAction::Files => {
                if let Some(tab) = self.tab_mut(id) {
                    tab.toggle_files();
                }
            }
            TabMenuAction::Log => self.toggle_log(id),
            TabMenuAction::Record => self.toggle_recording(id),
            TabMenuAction::RunScript(name) => {
                if let Some(dir) = self.scripts_dir()
                    && let Some(tab) = self.tab_mut(id)
                {
                    tab.run_script(&dir.join(name));
                }
            }
            TabMenuAction::StopScript => {
                if let Some(tab) = self.tab_mut(id) {
                    tab.stop_script();
                }
            }
            TabMenuAction::OpenScriptsFolder => self.open_scripts_folder(),
            TabMenuAction::Reconnect => {
                if let Some(tab) = self.tab_mut(id) {
                    tab.reconnect();
                }
            }
            TabMenuAction::TmuxNewWindow => {
                let sender = self.tab(id).and_then(|tab| tab.session_sender());
                if let Some(sender) = sender {
                    let cwd = self.tab(id).and_then(|tab| match &tab.tmux_role {
                        TmuxRole::Pane { cwd, .. } if !cwd.is_empty() => Some(cwd.clone()),
                        _ => None,
                    });
                    sender.send(SessionCommand::TmuxAction(TmuxClientAction::NewWindow {
                        cwd,
                    }));
                }
            }
            TabMenuAction::TmuxDetach => {
                if let Some(tab) = self.tab_mut(id)
                    && let Some(sender) = tab.session_sender()
                {
                    sender.send(SessionCommand::ExitTmuxControlMode { force: false });
                }
            }
            TabMenuAction::TmuxSyncPanes => {
                let sync_target = self.tab(id).and_then(|tab| match &tab.tmux_role {
                    TmuxRole::Pane {
                        sender, window_id, ..
                    } => Some((sender.clone(), *window_id)),
                    _ => None,
                });
                if let Some((sender, window_id)) = sync_target {
                    sender.send(SessionCommand::TmuxAction(
                        TmuxClientAction::ToggleSyncPanes { window: window_id },
                    ));
                    self.notify(
                        format!("Toggled synchronize-panes on tmux window {window_id}."),
                        false,
                    );
                }
            }
            TabMenuAction::Close => self.request_close(id),
        }
    }

    /// Stop the tab's log, or ask where to start one.
    fn toggle_log(&mut self, id: TileId) {
        let log_dir = self.config.settings.log_dir.trim();
        let dir = if log_dir.is_empty() {
            theme::download_dir(&self.config.settings)
        } else {
            rooste_core::expand_tilde(log_dir)
        };
        let Some(tab) = self.tab_mut(id) else { return };
        if tab.log.is_some() {
            tab.stop_log();
            return;
        }
        let suggested = dir.join(crate::tab::log_file_name(&tab.name));
        let ctx = tab.ctx.clone();
        self.log_target = Some(id);
        self.picker.open(
            &ctx,
            PickId::SessionLog,
            PickKind::SaveFile,
            &suggested.display().to_string(),
        );
    }

    /// Stop the tab's recording, or ask where to start one.
    fn toggle_recording(&mut self, id: TileId) {
        let log_dir = self.config.settings.log_dir.trim();
        let dir = if log_dir.is_empty() {
            theme::download_dir(&self.config.settings)
        } else {
            rooste_core::expand_tilde(log_dir)
        };
        let Some(tab) = self.tab_mut(id) else { return };
        if tab.recording.is_some() {
            tab.stop_recording();
            return;
        }
        let suggested = dir.join(crate::recording::recording_file_name(&tab.name));
        let ctx = tab.ctx.clone();
        self.recording_target = Some(id);
        self.picker.open(
            &ctx,
            PickId::SessionRecording,
            PickKind::SaveFile,
            &suggested.display().to_string(),
        );
    }

    /// The folder scripts are kept in.
    fn scripts_dir(&self) -> Option<PathBuf> {
        crate::scripting::scripts_dir(self.config_path.as_deref())
    }

    /// Open the scripts folder in the file manager, creating it (with an
    /// example script) the first time.
    fn open_scripts_folder(&mut self) {
        let Some(dir) = self.scripts_dir() else {
            self.notify("Scripts need a configuration folder.", true);
            return;
        };
        if let Err(e) = crate::scripting::ensure_scripts_dir(&dir) {
            self.notify(format!("Could not create {}: {e}", dir.display()), true);
            return;
        }
        let path = dir.display().to_string().replace('\\', "/");
        let url = if path.starts_with('/') {
            format!("file://{path}")
        } else {
            format!("file:///{path}")
        };
        if let Some(ctx) = &self.ctx {
            ctx.open_url(egui::OpenUrl::new_tab(url));
        }
        self.notify(format!("Scripts are in {}", dir.display()), false);
    }

    fn toggle_broadcast(&mut self) {
        self.broadcast = !self.broadcast;
        let count = self
            .tabs()
            .filter(|t| t.state == TabState::Connected)
            .count();
        if self.broadcast {
            self.notify(
                format!("Broadcasting input to {count} connected tabs."),
                false,
            );
        } else {
            self.notify("Stopped broadcasting input.", false);
        }
    }

    fn run_command(&mut self, ctx: &egui::Context, command: Command) {
        match command {
            Command::Connect(id) => self.connect_profile(ctx, &id),
            Command::NewSession => self.handle_manager(ctx, ManagerAction::New),
            Command::QuickConnect => self.dialog = Some(Dialog::QuickConnect),
            Command::ImportPutty => self.import(true),
            Command::ImportSshConfig => self.import(false),
            Command::Settings => {
                self.dialog = Some(Dialog::Settings(SettingsEditor {
                    settings: self.config.settings.clone(),
                }))
            }
            Command::DuplicateTab => self.duplicate_active(),
            Command::ToggleFiles => {
                if let Some(id) = self.active_tab_id()
                    && let Some(tab) = self.tab_mut(id)
                {
                    tab.toggle_files();
                }
            }
            Command::SplitRight => self.split_active(LinearDir::Horizontal),
            Command::SplitDown => self.split_active(LinearDir::Vertical),
            Command::ToggleBroadcast => self.toggle_broadcast(),
            Command::ToggleSessionManager => self.show_manager = !self.show_manager,
            Command::ToggleLog => {
                if let Some(id) = self.active_tab_id() {
                    self.toggle_log(id);
                }
            }
            Command::RunScript(name) => {
                if let Some(dir) = self.scripts_dir()
                    && let Some(id) = self.active_tab_id()
                    && let Some(tab) = self.tab_mut(id)
                {
                    tab.run_script(&dir.join(name));
                }
            }
            Command::OpenScriptsFolder => self.open_scripts_folder(),
            Command::ToggleRecording => {
                if let Some(id) = self.active_tab_id() {
                    self.toggle_recording(id);
                }
            }
            Command::Find => {
                if let Some(id) = self.active_tab_id()
                    && let Some(tab) = self.tab_mut(id)
                {
                    tab.open_find();
                }
            }
            Command::ZoomIn | Command::ZoomOut | Command::ResetZoom => {
                if let Some(id) = self.active_tab_id()
                    && let Some(tab) = self.tab_mut(id)
                {
                    match command {
                        Command::ZoomIn => tab.view.zoom_in(),
                        Command::ZoomOut => tab.view.zoom_out(),
                        _ => tab.view.reset_zoom(),
                    }
                }
            }
            Command::TmuxNewWindow => {
                if let Some(id) = self.active_tab_id()
                    && let Some(tab) = self.tab_mut(id)
                    && let Some(sender) = tab.session_sender()
                {
                    let cwd = match &tab.tmux_role {
                        TmuxRole::Pane { cwd, .. } if !cwd.is_empty() => Some(cwd.clone()),
                        _ => None,
                    };
                    sender.send(SessionCommand::TmuxAction(TmuxClientAction::NewWindow {
                        cwd,
                    }));
                }
            }
            Command::TmuxDetach => {
                if let Some(id) = self.active_tab_id() {
                    self.tab_menu_action(id, TabMenuAction::TmuxDetach);
                }
            }
            Command::TmuxToggleSyncPanes => {
                if let Some(id) = self.active_tab_id() {
                    self.tab_menu_action(id, TabMenuAction::TmuxSyncPanes);
                }
            }
            Command::CloseTab => {
                if let Some(id) = self.active_tab_id() {
                    self.request_close(id);
                }
            }
        }
    }

    fn import(&mut self, from_putty: bool) {
        let (source, result) = if from_putty {
            let result = match &self.imports.putty_dir {
                Some(dir) => putty::import_dir(dir),
                None => putty::import_default(),
            };
            ("PuTTY", result)
        } else {
            let result = match &self.imports.ssh_config {
                Some(path) => ssh_config::import_file(path),
                None => ssh_config::import_default(),
            };
            ("~/.ssh/config", result)
        };
        match result {
            Ok(profiles) => {
                let found = profiles.len();
                let added = self.config.merge_imported(profiles);
                self.save_config();
                let skipped = found - added;
                let text = match (added, skipped) {
                    (0, 0) => format!("No sessions found in {source}."),
                    (added, 0) => format!("Imported {added} sessions from {source}."),
                    (added, skipped) => format!(
                        "Imported {added} sessions from {source} ({skipped} already existed)."
                    ),
                };
                self.notify(text, false);
            }
            Err(e) => self.notify(format!("Could not import from {source}: {e}"), true),
        }
    }

    fn apply_settings(&mut self, settings: Settings) {
        self.config.settings = settings;
        self.save_config();
        let settings = self.config.settings.clone();
        for (_, tile) in self.tree.tiles.iter_mut() {
            if let Tile::Pane(tab) = tile {
                tab.apply_settings(&settings);
            }
        }
    }

    fn handle_manager(&mut self, ctx: &egui::Context, action: ManagerAction) {
        match action {
            ManagerAction::QuickConnect => self.dialog = Some(Dialog::QuickConnect),
            ManagerAction::Import { putty } => self.import(putty),
            ManagerAction::New | ManagerAction::NewInFolder(_) => {
                let folder = match action {
                    ManagerAction::NewInFolder(folder) => folder,
                    _ => String::new(),
                };
                let profile = Profile {
                    user: self.config.settings.default_user.clone(),
                    folder,
                    ..Profile::default()
                };
                self.serial_ports = rooste_protocols::serial::available_ports();
                self.dialog = Some(Dialog::EditProfile(Box::new(ProfileEditor::new(
                    profile, true,
                ))));
            }
            ManagerAction::Connect(id) => self.connect_profile(ctx, &id),
            ManagerAction::Edit(id) => {
                if let Some(profile) = self.config.profile(&id) {
                    self.serial_ports = rooste_protocols::serial::available_ports();
                    self.dialog = Some(Dialog::EditProfile(Box::new(ProfileEditor::new(
                        profile.clone(),
                        false,
                    ))));
                }
            }
            ManagerAction::Duplicate(id) => {
                if let Some(copy) = self.config.profile(&id).map(Profile::duplicate) {
                    self.manager.selected = Some(copy.id.clone());
                    self.config.upsert_profile(copy);
                    self.save_config();
                }
            }
            ManagerAction::Delete(id) => self.dialog = Some(Dialog::DeleteProfile(id)),
        }
    }

    /// Draw one frame.
    pub fn show(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.picker.poll();
        if let Some(path) = self.picker.take(PickId::SessionLog)
            && let Some(id) = self.log_target.take()
            && let Some(tab) = self.tab_mut(id)
        {
            tab.start_log(&path);
        }
        if let Some(path) = self.picker.take(PickId::SessionRecording)
            && let Some(id) = self.recording_target.take()
            && let Some(tab) = self.tab_mut(id)
        {
            tab.start_recording(&path);
        }
        // Before the terminal reads keys, so these keys don't reach the shell.
        self.terminal_shortcuts(&ctx);
        if self.applied_theme != Some(self.config.settings.theme) {
            ctx.set_visuals(theme::visuals(self.config.settings.theme));
            ctx.all_styles_mut(theme::spacing);
            self.applied_theme = Some(self.config.settings.theme);
        }

        self.ctx.get_or_insert_with(|| ctx.clone());
        self.update_result();

        // Session events and tab actions.
        for id in self.tab_ids() {
            let actions = match self.tab_mut(id) {
                Some(tab) => {
                    tab.poll(&ctx);
                    tab.take_actions()
                }
                None => continue,
            };
            for action in actions {
                match action {
                    TabAction::EnableLegacy { profile_id } => {
                        if let Some(profile) = profile_id
                            .and_then(|id| self.config.profiles.iter_mut().find(|p| p.id == id))
                        {
                            profile.allow_legacy = true;
                            self.save_config();
                        }
                    }
                    TabAction::Notify { text, error } => self.notify(text, error),
                    TabAction::RememberDownloadFolder(dir) => {
                        self.config.settings.last_download_to = dir.display().to_string();
                        self.save_config();
                        let settings = self.config.settings.clone();
                        for (_, tile) in self.tree.tiles.iter_mut() {
                            if let Tile::Pane(tab) = tile {
                                tab.apply_settings(&settings);
                            }
                        }
                    }
                    TabAction::SavePassword {
                        profile_id,
                        password,
                    } => {
                        if let Err(e) = self.secrets.set(&profile_id, &password) {
                            self.notify(e, true);
                        }
                    }
                    TabAction::TmuxTopologySynced {
                        gateway_tab_id,
                        session_id,
                        session_name,
                        windows,
                    } => {
                        self.sync_tmux_topology(
                            &ctx,
                            gateway_tab_id,
                            session_id,
                            session_name,
                            windows,
                        );
                    }
                    TabAction::TmuxPaneOutput {
                        gateway_tab_id,
                        pane_id,
                        bytes,
                    } => {
                        if let Some(tile_id) = self.find_tmux_pane_tile(gateway_tab_id, pane_id)
                            && let Some(tab) = self.tab_mut(tile_id)
                        {
                            tab.receive_tmux_output(&bytes);
                        }
                    }
                    TabAction::TmuxNotification {
                        gateway_tab_id,
                        notification,
                    } => {
                        self.handle_tmux_notification(gateway_tab_id, notification);
                    }
                    TabAction::TmuxPaneMetadata {
                        gateway_tab_id,
                        pane_id,
                        title,
                        cwd,
                    } => {
                        if let Some(tile_id) = self.find_tmux_pane_tile(gateway_tab_id, pane_id)
                            && let Some(tab) = self.tab_mut(tile_id)
                        {
                            if !title.is_empty() {
                                tab.remote_title = Some(title);
                            }
                            if let TmuxRole::Pane { cwd: pane_cwd, .. } = &mut tab.tmux_role
                                && !cwd.is_empty()
                            {
                                *pane_cwd = cwd;
                            }
                        }
                    }
                    TabAction::TmuxDetached {
                        gateway_tab_id,
                        reason,
                    } => {
                        self.handle_tmux_detached(gateway_tab_id, reason);
                    }
                }
            }
        }

        self.menu_bar(ui);
        self.update_banner_ui(ui);
        self.status_bar(ui);
        if self.show_manager {
            let mut action = None;
            egui::Panel::left("session-manager")
                .frame(theme::side_panel_frame(&ctx))
                .resizable(true)
                .default_size(240.0)
                .show(ui, |ui| {
                    action = manager::show(ui, &mut self.manager, &self.config.profiles);
                });
            if let Some(action) = action {
                self.handle_manager(&ctx, action);
            }
        }

        // No margin (tabs reach the edges), but the theme's background: the
        // window behind it is nearly black.
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(ctx.global_style().visuals.panel_fill))
            .show(ui, |ui| {
                if self.tree.is_empty() {
                    self.welcome(ui);
                } else {
                    let mut behavior = TabBehavior {
                        close_requests: Vec::new(),
                        menu_actions: Vec::new(),
                        scripts_dir: self.scripts_dir(),
                        selected_tab: std::cell::Cell::new(None),
                    };
                    let broadcast = self.broadcast;
                    for (_, tile) in self.tree.tiles.iter_mut() {
                        if let Tile::Pane(tab) = tile {
                            let is_tmux_gw = matches!(tab.tmux_role, TmuxRole::Gateway { .. });
                            tab.broadcast =
                                broadcast && tab.state == TabState::Connected && !is_tmux_gw;
                        }
                    }
                    crate::terminal_view::set_focus_pending(
                        ui.ctx(),
                        self.tabs().any(|tab| tab.view.focus_requested),
                    );
                    self.tree.ui(&mut behavior, ui);
                    self.follow_tab_switches();
                    self.deliver_broadcast();
                    for id in behavior.close_requests {
                        self.request_close(id);
                    }
                    for (id, action) in behavior.menu_actions {
                        self.tab_menu_action(id, action);
                    }
                }
            });

        self.dialogs(&ctx);
        self.shortcuts(&ctx);
        self.update_window_title(&ctx);
    }

    /// Send input typed in broadcasting tabs to every connected tab.
    fn deliver_broadcast(&mut self) {
        let mut input = Vec::new();
        for (_, tile) in self.tree.tiles.iter_mut() {
            if let Tile::Pane(tab) = tile {
                if !matches!(tab.tmux_role, TmuxRole::Gateway { .. }) {
                    input.append(&mut tab.outbox);
                } else {
                    tab.outbox.clear();
                }
            }
        }
        if input.is_empty() {
            return;
        }
        for (_, tile) in self.tree.tiles.iter_mut() {
            if let Tile::Pane(tab) = tile
                && !matches!(tab.tmux_role, TmuxRole::Gateway { .. })
            {
                tab.send_input(input.clone());
            }
        }
    }

    fn menu_bar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        egui::Panel::top("menu").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("Session", |ui| {
                    if ui.button("New session…").clicked() {
                        self.handle_manager(&ctx, ManagerAction::New);
                        ui.close();
                    }
                    if ui.button("Quick connect…").clicked() {
                        self.dialog = Some(Dialog::QuickConnect);
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Import from PuTTY").clicked() {
                        self.import(true);
                        ui.close();
                    }
                    if ui.button("Import from ~/.ssh/config").clicked() {
                        self.import(false);
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Settings…").clicked() {
                        self.dialog = Some(Dialog::Settings(SettingsEditor {
                            settings: self.config.settings.clone(),
                        }));
                        ui.close();
                    }
                });
                ui.menu_button("Tab", |ui| {
                    let has_tab = self.active_tab_id().is_some();
                    if ui
                        .add_enabled(
                            has_tab,
                            egui::Button::new("Find…").shortcut_text("Ctrl+Shift+F"),
                        )
                        .clicked()
                    {
                        self.run_command(&ctx, Command::Find);
                        ui.close();
                    }
                    let logging = self
                        .active_tab_id()
                        .and_then(|id| self.tree.tiles.get_pane(&id))
                        .is_some_and(|tab| tab.log.is_some());
                    let log_label = if logging {
                        "Stop logging"
                    } else {
                        "Start logging…"
                    };
                    if ui
                        .add_enabled(has_tab, egui::Button::new(log_label))
                        .clicked()
                    {
                        self.run_command(&ctx, Command::ToggleLog);
                        ui.close();
                    }
                    let recording = self
                        .active_tab_id()
                        .and_then(|id| self.tree.tiles.get_pane(&id))
                        .is_some_and(|tab| tab.recording.is_some());
                    let record_label = if recording {
                        "Stop recording"
                    } else {
                        "Record session…"
                    };
                    if ui
                        .add_enabled(has_tab, egui::Button::new(record_label))
                        .clicked()
                    {
                        self.run_command(&ctx, Command::ToggleRecording);
                        ui.close();
                    }
                    for (label, command) in [
                        ("Files (SFTP)", Command::ToggleFiles),
                        ("Duplicate tab", Command::DuplicateTab),
                        ("Split right", Command::SplitRight),
                        ("Split down", Command::SplitDown),
                    ] {
                        if ui.add_enabled(has_tab, egui::Button::new(label)).clicked() {
                            self.run_command(&ctx, command);
                            ui.close();
                        }
                    }
                    if ui
                        .checkbox(&mut self.broadcast, "Broadcast input to all tabs")
                        .clicked()
                    {
                        self.broadcast = !self.broadcast;
                        self.toggle_broadcast();
                        ui.close();
                    }
                    if self.has_active_tmux() {
                        ui.separator();
                        if ui.button("tmux: New window").clicked() {
                            self.run_command(&ctx, Command::TmuxNewWindow);
                            ui.close();
                        }
                        if ui.button("tmux: Synchronize panes").clicked() {
                            self.run_command(&ctx, Command::TmuxToggleSyncPanes);
                            ui.close();
                        }
                        if ui.button("tmux: Detach session").clicked() {
                            self.run_command(&ctx, Command::TmuxDetach);
                            ui.close();
                        }
                    }
                    ui.separator();
                    if ui
                        .add_enabled(has_tab, egui::Button::new("Close tab"))
                        .clicked()
                    {
                        self.run_command(&ctx, Command::CloseTab);
                        ui.close();
                    }
                });
                ui.menu_button("Tools", |ui| {
                    if ui.button("Key generator…").clicked() {
                        self.dialog = Some(Dialog::KeyGenerator(Box::default()));
                        ui.close();
                    }
                    if ui.button("Convert key (PuTTY / OpenSSH)…").clicked() {
                        self.dialog = Some(Dialog::KeyConverter(Box::default()));
                        ui.close();
                    }
                    if ui.button("SSH agent…").clicked() {
                        let mut window = AgentWindow::default();
                        self.agent_action(&mut window, AgentAction::Refresh);
                        self.dialog = Some(Dialog::Agent(Box::new(window)));
                        ui.close();
                    }
                });
                ui.menu_button("View", |ui| {
                    if ui
                        .checkbox(&mut self.show_manager, "Session manager")
                        .clicked()
                    {
                        ui.close();
                    }
                    if ui.button("Command palette…").clicked() {
                        self.dialog = Some(Dialog::Palette {
                            query: String::new(),
                        });
                        ui.close();
                    }
                    ui.separator();
                    let has_tab = self.active_tab_id().is_some();
                    for (label, shortcut, command) in [
                        ("Zoom in", "Ctrl +", Command::ZoomIn),
                        ("Zoom out", "Ctrl −", Command::ZoomOut),
                        ("Reset zoom", "Ctrl 0", Command::ResetZoom),
                    ] {
                        let button = egui::Button::new(label).shortcut_text(shortcut);
                        if ui.add_enabled(has_tab, button).clicked() {
                            self.run_command(&ctx, command);
                            ui.close();
                        }
                    }
                });
                ui.menu_button("Help", |ui| {
                    if ui.button("Check for updates").clicked() {
                        self.check_for_updates(true);
                        ui.close();
                    }
                    if ui.button("Project page").clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(
                            "https://github.com/rooste-ssh/rooste-ssh",
                        ));
                        ui.close();
                    }
                    ui.separator();
                    ui.weak(format!("Rooste SSH {}", env!("CARGO_PKG_VERSION")));
                });
            });
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let Some(notice) = self.notice.clone() else {
            return;
        };
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                if notice.error {
                    ui.colored_label(ui.visuals().error_fg_color, &notice.text);
                } else {
                    ui.label(&notice.text);
                }
                if let Some((label, url)) = &notice.link {
                    ui.hyperlink_to(label, url);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("×").on_hover_text("Dismiss").clicked() {
                        self.notice = None;
                    }
                });
            });
        });
    }

    fn welcome(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(48.0);
            ui.vertical_centered(|ui| {
                ui.heading("Rooste SSH");
                ui.add_space(4.0);
                ui.label(RichText::new("Double-click a saved session, or connect quickly:").weak());
            });
            ui.add_space(24.0);
            // Center the form, with at least a small gap from the panel edge.
            let indent = ((ui.available_width() - 420.0) / 2.0).max(16.0);
            ui.horizontal(|ui| {
                ui.add_space(indent);
                ui.vertical(|ui| {
                    let mut submit = self.quick_form(ui);
                    ui.add_space(8.0);
                    // Centered under the form.
                    let width = ui.min_rect().width();
                    let size = egui::vec2(width, 36.0);
                    let layout = egui::Layout::top_down(egui::Align::Center);
                    ui.allocate_ui_with_layout(size, layout, |ui| {
                        let connect =
                            egui::Button::new("Connect").min_size(egui::vec2(140.0, 32.0));
                        submit |= crate::theme::primary(ui, |ui| ui.add(connect)).clicked();
                    });
                    if submit {
                        self.quick_connect(&ctx);
                    }
                });
            });
        });
    }

    /// The quick-connect fields. Returns true when the user presses Enter in
    /// one; the caller adds the Connect button.
    fn quick_form(&mut self, ui: &mut Ui) -> bool {
        let mut submit = false;
        egui::Grid::new("quick-connect")
            .num_columns(2)
            .spacing(crate::theme::FORM_SPACING)
            .show(ui, |ui| {
                let mut field = |ui: &mut Ui, label: &str, value: &mut String, password: bool| {
                    let label = ui.label(label);
                    let edit = egui::TextEdit::singleline(value)
                        .margin(crate::theme::FIELD_MARGIN)
                        .password(password)
                        .desired_width(crate::theme::FIELD_WIDTH);
                    let response = ui.add(edit).labelled_by(label.id);
                    submit |=
                        response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.end_row();
                };
                field(ui, "Host", &mut self.quick.host, false);
                field(ui, "Port", &mut self.quick.port, false);
                field(ui, "User", &mut self.quick.user, false);
                field(ui, "Password", &mut self.quick.password, true);
                let key_file = path_row(
                    ui,
                    "Key file",
                    &mut self.quick.key_file,
                    &mut self.picker,
                    PickId::QuickKeyFile,
                    PickKind::OpenFile,
                );
                submit |= key_file.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.label("");
                ui.checkbox(&mut self.quick.use_agent, "Use SSH agent");
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut self.quick.allow_legacy, "Allow legacy algorithms")
                    .on_hover_text("Offer outdated SHA-1 and CBC algorithms for old devices");
                ui.end_row();
            });
        if let Some(error) = &self.quick_error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        submit
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        // Host keys and prompts from sessions take priority.
        for id in self.tab_ids() {
            let Some(tab) = self.tab_mut(id) else {
                continue;
            };
            if let Some(key) = tab.pending_host_key().cloned() {
                match dialogs::host_key(ctx, &key) {
                    Decision::Accept(()) => tab.answer_host_key(true),
                    Decision::Cancel => tab.answer_host_key(false),
                    Decision::Pending => {}
                }
                return;
            }
            if let Some(prompt) = tab.prompt.as_mut() {
                let decision = dialogs::prompt(ctx, &prompt.kind, prompt.retry, &mut prompt.input);
                match decision {
                    Decision::Accept(secret) => tab.answer_prompt(Some(secret)),
                    Decision::Cancel => tab.answer_prompt(None),
                    Decision::Pending => {}
                }
                return;
            }
        }

        let has_tmux = self.has_active_tmux();
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        let keep = match &mut dialog {
            Dialog::EditProfile(editor) => {
                let scripts = self
                    .scripts_dir()
                    .map(|dir| crate::scripting::list_scripts(&dir))
                    .unwrap_or_default();
                match editor.show(
                    ctx,
                    &self.config.folders(),
                    &self.serial_ports,
                    &scripts,
                    &mut self.picker,
                ) {
                    Decision::Pending => true,
                    Decision::Cancel => false,
                    Decision::Accept((profile, password)) => {
                        if profile.save_password && !password.is_empty() {
                            if let Err(e) = self.secrets.set(&profile.id, &password) {
                                self.notify(e, true);
                            }
                        } else if !profile.save_password {
                            self.secrets.delete(&profile.id);
                        }
                        self.manager.selected = Some(profile.id.clone());
                        self.config.upsert_profile(profile);
                        self.save_config();
                        false
                    }
                }
            }
            Dialog::Settings(editor) => match editor.show(ctx, &mut self.picker) {
                Decision::Pending => true,
                Decision::Cancel => false,
                Decision::Accept(settings) => {
                    self.apply_settings(settings);
                    false
                }
            },
            Dialog::DeleteProfile(id) => {
                let name = self
                    .config
                    .profile(id)
                    .map(Profile::display_name)
                    .unwrap_or_default();
                match dialogs::confirm(
                    ctx,
                    "delete-profile",
                    "Delete session",
                    &format!("Delete the saved session \"{name}\"?"),
                    "Delete",
                ) {
                    Decision::Pending => true,
                    Decision::Cancel => false,
                    Decision::Accept(()) => {
                        self.secrets.delete(id);
                        self.config.remove_profile(id);
                        self.save_config();
                        false
                    }
                }
            }
            Dialog::CloseTab(id) => {
                let id = *id;
                match dialogs::confirm(
                    ctx,
                    "close-tab",
                    "Close tab",
                    "This tab has an active connection. Close it?",
                    "Close",
                ) {
                    Decision::Pending => true,
                    Decision::Cancel => false,
                    Decision::Accept(()) => {
                        self.close_tab(id);
                        false
                    }
                }
            }
            Dialog::Palette { query } => {
                let scripts = self
                    .scripts_dir()
                    .map(|dir| crate::scripting::list_scripts(&dir))
                    .unwrap_or_default();
                let commands = command_palette::commands_with_tmux(
                    &self.config.profiles,
                    self.active_tab_id().is_some(),
                    has_tmux,
                    &scripts,
                );
                match command_palette::show(ctx, query, &commands) {
                    Decision::Pending => true,
                    Decision::Cancel => false,
                    Decision::Accept(command) => {
                        self.run_command(ctx, command);
                        false
                    }
                }
            }
            Dialog::KeyGenerator(generator) => {
                generator.show(ctx, &mut self.picker) == Decision::Pending
            }
            Dialog::KeyConverter(converter) => {
                converter.show(ctx, &mut self.picker) == Decision::Pending
            }
            Dialog::Agent(window) => {
                let mut use_agent = self.config.settings.use_builtin_agent;
                let (decision, action) = window.show(
                    ctx,
                    self.builtin_agent.as_ref(),
                    &mut use_agent,
                    &mut self.picker,
                );
                if use_agent != self.config.settings.use_builtin_agent {
                    self.config.settings.use_builtin_agent = use_agent;
                    self.save_config();
                }
                if let Some(action) = action {
                    self.agent_action(window, action);
                }
                decision == Decision::Pending
            }
            Dialog::QuickConnect => {
                let mut keep = true;
                egui::Modal::new(egui::Id::unique("quick-connect-dialog"))
                    .frame(crate::theme::dialog_frame(ctx))
                    .show(ctx, |ui| {
                        ui.heading("Quick connect");
                        ui.add_space(10.0);
                        let mut submit = self.quick_form(ui);
                        let mut cancel = false;
                        crate::dialogs::footer(ui, |ui| {
                            cancel = ui.button("Cancel").clicked();
                            submit |= crate::theme::primary_button(ui, "Connect").clicked();
                        });
                        if cancel || (submit && self.quick_connect(ctx)) {
                            keep = false;
                        }
                    });
                keep
            }
        };
        if keep && self.dialog.is_none() {
            self.dialog = Some(dialog);
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, KeyboardShortcut, Modifiers};
        // Ctrl+Shift on Linux and Windows, ⌘⇧ on macOS.
        let ctrl_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(ctrl_shift, Key::T))) {
            self.dialog = Some(Dialog::QuickConnect);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(ctrl_shift, Key::P))) {
            self.dialog = Some(Dialog::Palette {
                query: String::new(),
            });
        }
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(ctrl_shift, Key::W)))
            && let Some(id) = self.active_tab_id()
        {
            self.request_close(id);
        }
    }

    /// Shortcuts the terminal would otherwise take as input: Ctrl + / Ctrl -
    /// / Ctrl 0 zoom the active tab's text, Ctrl+Shift+F finds in it.
    fn terminal_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, KeyboardShortcut, Modifiers};
        // egui would zoom the whole interface on these keys.
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let pressed = |key| {
            ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, key)))
        };
        let find = KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::F);
        if ctx.input_mut(|i| i.consume_shortcut(&find)) {
            self.run_command(ctx, Command::Find);
        }
        let command = if pressed(Key::Plus) || pressed(Key::Equals) {
            Command::ZoomIn
        } else if pressed(Key::Minus) {
            Command::ZoomOut
        } else if pressed(Key::Num0) {
            Command::ResetZoom
        } else {
            return;
        };
        self.run_command(ctx, command);
    }

    fn update_window_title(&mut self, ctx: &egui::Context) {
        let title = match self
            .active_tab_id()
            .and_then(|id| self.tree.tiles.get_pane(&id))
        {
            Some(tab) => format!(
                "{} — Rooste SSH",
                tab.remote_title.clone().unwrap_or_else(|| tab.name.clone())
            ),
            None => "Rooste SSH".into(),
        };
        if title != self.window_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.window_title = title;
        }
    }
}

/// Replace `old` with `new` among a container's children.
fn replace_child(container: &mut Container, old: TileId, new: TileId) {
    let children = match container {
        Container::Tabs(tabs) => {
            if tabs.active == Some(old) {
                tabs.active = Some(new);
            }
            &mut tabs.children
        }
        Container::Linear(linear) => &mut linear.children,
        Container::Grid(_) => return,
    };
    for child in children.iter_mut() {
        if *child == old {
            *child = new;
        }
    }
}

impl Default for RoosteApp {
    fn default() -> Self {
        Self::new()
    }
}

impl eframe::App for RoosteApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_form_validation() {
        let mut form = ConnectForm {
            user: "me".into(),
            ..ConnectForm::default()
        };
        assert_eq!(form.request().unwrap_err(), "Enter a host name.");
        form.host = " example.com ".into();
        form.port = "0".into();
        assert!(form.request().unwrap_err().starts_with("Port"));
        form.port = "2222".into();
        form.password = "pw".into();
        let request = form.request().unwrap();
        assert_eq!(request.host, "example.com");
        assert_eq!(request.port, 2222);
        assert_eq!(request.password.as_deref(), Some("pw"));
        assert!(request.prompt_for_password);
    }
}
