//! The SFTP file panel: a worker task owning the SFTP session, and the UI
//! showing a directory with transfers.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use egui::{RichText, Ui};

use crate::picker::{PickId, PickKind, Picker};
use rooste_sftp::{Entry, Sftp, format_size};
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum FilesCommand {
    List(String),
    Download {
        id: u64,
        remote: String,
        local: PathBuf,
    },
    /// Download a folder and everything in it into a new local folder.
    DownloadTree {
        id: u64,
        remote: String,
        local: PathBuf,
    },
    Upload {
        id: u64,
        local: PathBuf,
        remote: String,
    },
    /// Upload a local folder and everything in it (merged into the remote
    /// folder if it exists).
    UploadTree {
        id: u64,
        local: PathBuf,
        remote: String,
    },
    Mkdir(String),
    Remove {
        path: String,
        dir: bool,
    },
    Rename {
        from: String,
        to: String,
    },
}

#[derive(Debug)]
pub enum FilesEvent {
    Listing {
        path: String,
        entries: Vec<Entry>,
    },
    Progress {
        id: u64,
        done: u64,
        total: u64,
    },
    Finished {
        id: u64,
        result: Result<(), String>,
    },
    /// A directory operation finished; `dir` should be listed again.
    Changed {
        dir: String,
    },
    Error(String),
}

/// The UI's side of an SFTP worker.
#[derive(Debug)]
pub struct FilesHandle {
    commands: mpsc::UnboundedSender<FilesCommand>,
    events: std_mpsc::Receiver<FilesEvent>,
    cancelled: Arc<Mutex<HashSet<u64>>>,
    pub home: String,
}

impl FilesHandle {
    fn send(&self, command: FilesCommand) {
        let _ = self.commands.send(command);
    }

    fn cancel(&self, id: u64) {
        self.cancelled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id);
    }
}

/// Start a worker for `sftp`. `wake` repaints the UI when events arrive.
pub fn spawn(sftp: Sftp, home: String, wake: Arc<dyn Fn() + Send + Sync>) -> FilesHandle {
    let (commands, mut rx) = mpsc::unbounded_channel::<FilesCommand>();
    let (tx, events) = std_mpsc::channel();
    let cancelled: Arc<Mutex<HashSet<u64>>> = Arc::default();
    let is_cancelled = cancelled.clone();
    let send = move |event: FilesEvent| {
        if tx.send(event).is_ok() {
            wake();
        }
    };
    tokio::spawn(async move {
        while let Some(command) = rx.recv().await {
            match command {
                FilesCommand::List(path) => match sftp.list(&path).await {
                    Ok(entries) => send(FilesEvent::Listing { path, entries }),
                    Err(e) => send(FilesEvent::Error(format!("{path}: {e}"))),
                },
                FilesCommand::Download { id, remote, local } => {
                    let result = sftp
                        .download(&remote, &local, |done, total| {
                            send(FilesEvent::Progress { id, done, total });
                            !is_cancelled
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .contains(&id)
                        })
                        .await;
                    send(FilesEvent::Finished {
                        id,
                        result: result.map(drop).map_err(|e| e.to_string()),
                    });
                }
                FilesCommand::DownloadTree { id, remote, local } => {
                    let result = download_tree(&sftp, &remote, &local, |done, total| {
                        send(FilesEvent::Progress { id, done, total });
                        !is_cancelled
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .contains(&id)
                    })
                    .await;
                    send(FilesEvent::Finished { id, result });
                }
                FilesCommand::Upload { id, local, remote } => {
                    let result = sftp
                        .upload(&local, &remote, |done, total| {
                            send(FilesEvent::Progress { id, done, total });
                            !is_cancelled
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .contains(&id)
                        })
                        .await;
                    send(FilesEvent::Finished {
                        id,
                        result: result.map(drop).map_err(|e| e.to_string()),
                    });
                    send(FilesEvent::Changed {
                        dir: rooste_sftp::parent(&remote),
                    });
                }
                FilesCommand::UploadTree { id, local, remote } => {
                    let result = upload_tree(&sftp, &local, &remote, |done, total| {
                        send(FilesEvent::Progress { id, done, total });
                        !is_cancelled
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .contains(&id)
                    })
                    .await;
                    send(FilesEvent::Finished { id, result });
                    send(FilesEvent::Changed {
                        dir: rooste_sftp::parent(&remote),
                    });
                }
                FilesCommand::Mkdir(path) => {
                    let result = sftp.mkdir(&path).await;
                    report(&send, &path, result);
                }
                FilesCommand::Remove { path, dir } => {
                    let result = if dir {
                        sftp.remove_dir(&path).await
                    } else {
                        sftp.remove_file(&path).await
                    };
                    report(&send, &path, result);
                }
                FilesCommand::Rename { from, to } => {
                    let result = sftp.rename(&from, &to).await;
                    report(&send, &from, result);
                }
            }
        }
    });
    FilesHandle {
        commands,
        events,
        cancelled,
        home,
    }
}

/// Download the remote folder `remote` into `local` (created), with all
/// its files and subfolders. Progress counts bytes over all files; it
/// returns false to cancel.
async fn download_tree(
    sftp: &Sftp,
    remote: &str,
    local: &Path,
    mut progress: impl FnMut(u64, u64) -> bool,
) -> Result<(), String> {
    // Find every file first, so progress has a total.
    let mut dirs = vec![(remote.to_owned(), local.to_owned())];
    let mut files = Vec::new();
    let mut next = 0;
    while let Some((dir, target)) = dirs.get(next).cloned() {
        next += 1;
        tokio::fs::create_dir_all(&target)
            .await
            .map_err(|e| format!("{}: {e}", target.display()))?;
        for entry in sftp.list(&dir).await.map_err(|e| format!("{dir}: {e}"))? {
            let to = target.join(&entry.name);
            if entry.is_dir() {
                dirs.push((entry.path, to));
            } else {
                files.push((entry.path, to, entry.size));
            }
        }
    }
    let total: u64 = files.iter().map(|(_, _, size)| size).sum();
    let count = files.len();
    let mut finished = 0;
    let mut failed = Vec::new();
    for (remote, local, size) in files {
        let mut keep_going = true;
        let result = sftp
            .download(&remote, &local, |done, _| {
                keep_going = progress(finished + done, total);
                keep_going
            })
            .await;
        if !keep_going {
            return Err("cancelled".into());
        }
        if let Err(e) = result {
            failed.push(format!("{remote}: {e}"));
        }
        finished += size;
    }
    progress(total, total);
    match failed.first() {
        None => Ok(()),
        Some(first) => Err(format!(
            "{} of {count} files failed, first: {first}",
            failed.len()
        )),
    }
}

/// Upload the local folder `local` to `remote` (created if missing), with
/// all its files and subfolders. Symlinked folders are skipped, as they
/// could loop. Progress counts bytes over all files; it returns false to
/// cancel.
async fn upload_tree(
    sftp: &Sftp,
    local: &Path,
    remote: &str,
    mut progress: impl FnMut(u64, u64) -> bool,
) -> Result<(), String> {
    let local_error = |path: &Path, e: std::io::Error| format!("{}: {e}", path.display());
    // Find every file first, so progress has a total.
    let mut dirs = vec![(local.to_owned(), remote.to_owned())];
    let mut files = Vec::new();
    let mut next = 0;
    while let Some((dir, target)) = dirs.get(next).cloned() {
        next += 1;
        sftp.ensure_dir(&target)
            .await
            .map_err(|e| format!("{target}: {e}"))?;
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .map_err(|e| local_error(&dir, e))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| local_error(&dir, e))?
        {
            let path = entry.path();
            let to = rooste_sftp::join(&target, &entry.file_name().to_string_lossy());
            let file_type = entry.file_type().await.map_err(|e| local_error(&path, e))?;
            if file_type.is_dir() {
                dirs.push((path, to));
            } else if let Ok(metadata) = tokio::fs::metadata(&path).await
                && metadata.is_file()
            {
                files.push((path, to, metadata.len()));
            }
        }
    }
    let total: u64 = files.iter().map(|(_, _, size)| size).sum();
    let count = files.len();
    let mut finished = 0;
    let mut failed = Vec::new();
    for (local, remote, size) in files {
        let mut keep_going = true;
        let result = sftp
            .upload(&local, &remote, |done, _| {
                keep_going = progress(finished + done, total);
                keep_going
            })
            .await;
        if !keep_going {
            return Err("cancelled".into());
        }
        if let Err(e) = result {
            failed.push(format!("{}: {e}", local.display()));
        }
        finished += size;
    }
    progress(total, total);
    match failed.first() {
        None => Ok(()),
        Some(first) => Err(format!(
            "{} of {count} files failed, first: {first}",
            failed.len()
        )),
    }
}

fn report(send: &impl Fn(FilesEvent), path: &str, result: Result<(), rooste_sftp::SftpError>) {
    match result {
        Ok(()) => send(FilesEvent::Changed {
            dir: rooste_sftp::parent(path),
        }),
        Err(e) => send(FilesEvent::Error(format!("{path}: {e}"))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Download,
    Upload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    Running,
    Done,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Transfer {
    pub id: u64,
    pub name: String,
    pub direction: Direction,
    pub done: u64,
    pub total: u64,
    pub state: TransferState,
    /// Where a download was saved.
    pub local: PathBuf,
}

/// A pending in-panel edit.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Edit {
    NewFolder(String),
    Rename { from: String, name: String },
    Delete(Entry),
}

/// Picked from an entry's right-click menu.
enum MenuAction {
    Open(String),
    Download,
    DownloadTo,
    DownloadInto(PathBuf),
}

/// State of the file panel in a tab.
#[derive(Debug)]
pub struct FilesPanel {
    pub handle: Option<FilesHandle>,
    pub cwd: String,
    pub entries: Vec<Entry>,
    /// Paths of the selected entries, in the order they were selected.
    pub selected: Vec<String>,
    /// Where Shift-click selection ranges start.
    anchor: Option<String>,
    pub transfers: Vec<Transfer>,
    pub error: Option<String>,
    pub download_dir: PathBuf,
    /// The folder last chosen with "Download to…".
    pub recent_dir: Option<PathBuf>,
    /// A folder newly chosen with "Download to…", for the app to remember.
    pub chosen_dir: Option<PathBuf>,
    /// Folder dialogs for "Download to…".
    pub picker: Picker,
    /// Entries waiting for the "Download to…" dialog.
    pending_download: Vec<Entry>,
    path_input: String,
    edit: Option<Edit>,
    next_id: u64,
}

impl FilesPanel {
    pub fn new(
        download_dir: PathBuf,
        recent_dir: Option<PathBuf>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            handle: None,
            cwd: String::new(),
            entries: Vec::new(),
            selected: Vec::new(),
            anchor: None,
            transfers: Vec::new(),
            error: None,
            download_dir,
            recent_dir,
            chosen_dir: None,
            picker: Picker::new(runtime),
            pending_download: Vec::new(),
            path_input: String::new(),
            edit: None,
            next_id: 0,
        }
    }

    pub fn attach(&mut self, handle: FilesHandle) {
        let home = handle.home.clone();
        self.handle = Some(handle);
        self.open(&home);
    }

    fn open(&mut self, path: &str) {
        if let Some(handle) = &self.handle {
            handle.send(FilesCommand::List(path.to_owned()));
        }
    }

    /// Download a file or folder to the download folder.
    pub fn download(&mut self, entry: &Entry) {
        let dir = self.download_dir.clone();
        self.download_into(entry, &dir);
    }

    /// Download a file, or a folder with everything in it, into `dir`.
    pub fn download_into(&mut self, entry: &Entry, dir: &Path) {
        let Some(handle) = &self.handle else { return };
        self.next_id += 1;
        let id = self.next_id;
        let local = unique_path(dir, &entry.name);
        let remote = entry.path.clone();
        let (command, name, total) = if entry.is_dir() {
            let command = FilesCommand::DownloadTree {
                id,
                remote,
                local: local.clone(),
            };
            (command, format!("{}/", entry.name), 0)
        } else {
            let command = FilesCommand::Download {
                id,
                remote,
                local: local.clone(),
            };
            (command, entry.name.clone(), entry.size)
        };
        handle.send(command);
        self.transfers.push(Transfer {
            id,
            name,
            direction: Direction::Download,
            done: 0,
            total,
            state: TransferState::Running,
            local,
        });
    }

    fn download_selected_into(&mut self, dir: &Path) {
        for entry in self.selected_entries() {
            self.download_into(&entry, dir);
        }
    }

    /// Ask for a folder, then download the selection into it.
    fn download_selected_to(&mut self, ctx: &egui::Context) {
        self.pending_download = self.selected_entries();
        let start = self.recent_dir.as_ref().unwrap_or(&self.download_dir);
        let start = start.display().to_string();
        self.picker
            .open(ctx, PickId::DownloadTo, PickKind::Folder, &start);
    }

    /// Select one entry, or toggle it (Ctrl) or select a range (Shift).
    fn click_entry(&mut self, index: usize, modifiers: egui::Modifiers) {
        let Some(path) = self.entries.get(index).map(|e| e.path.clone()) else {
            return;
        };
        let anchor = self
            .anchor
            .as_ref()
            .and_then(|a| self.entries.iter().position(|e| &e.path == a));
        if modifiers.command {
            if let Some(i) = self.selected.iter().position(|p| *p == path) {
                self.selected.remove(i);
            } else {
                self.selected.push(path.clone());
            }
            self.anchor = Some(path);
        } else if let (true, Some(anchor)) = (modifiers.shift, anchor) {
            let (from, to) = (anchor.min(index), anchor.max(index));
            self.selected = self.entries[from..=to]
                .iter()
                .map(|e| e.path.clone())
                .collect();
        } else {
            self.selected = vec![path.clone()];
            self.anchor = Some(path);
        }
    }

    pub fn upload(&mut self, local: &Path) {
        let Some(handle) = &self.handle else { return };
        let Some(name) = local.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return;
        };
        self.next_id += 1;
        let id = self.next_id;
        let remote = rooste_sftp::join(&self.cwd, &name);
        let local = local.to_owned();
        let (command, name, total) = if local.is_dir() {
            let command = FilesCommand::UploadTree {
                id,
                local: local.clone(),
                remote,
            };
            (command, format!("{name}/"), 0)
        } else {
            let total = std::fs::metadata(&local).map(|m| m.len()).unwrap_or(0);
            let command = FilesCommand::Upload {
                id,
                local: local.clone(),
                remote,
            };
            (command, name, total)
        };
        handle.send(command);
        self.transfers.push(Transfer {
            id,
            name,
            direction: Direction::Upload,
            done: 0,
            total,
            state: TransferState::Running,
            local,
        });
    }

    pub fn poll(&mut self) {
        let Some(handle) = &self.handle else { return };
        let events: Vec<FilesEvent> = handle.events.try_iter().collect();
        for event in events {
            match event {
                FilesEvent::Listing { path, entries } => {
                    self.path_input = path.clone();
                    self.cwd = path;
                    self.entries = entries;
                    self.selected.clear();
                    self.anchor = None;
                    self.error = None;
                }
                FilesEvent::Progress { id, done, total } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                        (t.done, t.total) = (done, total);
                    }
                }
                FilesEvent::Finished { id, result } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                        t.state = match result {
                            Ok(()) => TransferState::Done,
                            Err(e) => TransferState::Failed(e),
                        };
                    }
                }
                FilesEvent::Changed { dir } => {
                    if dir == self.cwd {
                        let cwd = self.cwd.clone();
                        self.open(&cwd);
                    }
                }
                FilesEvent::Error(message) => self.error = Some(message),
            }
        }
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        if self.handle.is_none() {
            ui.label(RichText::new("Starting SFTP…").italics());
            return;
        }
        // Files dropped onto the window are uploaded here.
        let dropped: Vec<PathBuf> = ui.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        for path in dropped {
            self.upload(&path);
        }
        self.picker.poll();
        for path in self.picker.take_all(PickId::UploadFiles) {
            self.upload(&path);
        }
        if let Some(path) = self.picker.take(PickId::UploadFolder) {
            self.upload(&path);
        }
        if let Some(dir) = self.picker.take(PickId::DownloadTo) {
            for entry in std::mem::take(&mut self.pending_download) {
                self.download_into(&entry, &dir);
            }
            self.recent_dir = Some(dir.clone());
            self.chosen_dir = Some(dir);
        }

        ui.horizontal(|ui| {
            if ui.button("⬆").on_hover_text("Parent folder").clicked() {
                let parent = rooste_sftp::parent(&self.cwd);
                self.open(&parent);
            }
            if ui.button("⟳").on_hover_text("Refresh").clicked() {
                let cwd = self.cwd.clone();
                self.open(&cwd);
            }
            let path = ui.add(
                egui::TextEdit::singleline(&mut self.path_input)
                    .margin(crate::theme::FIELD_MARGIN)
                    .desired_width(f32::INFINITY),
            );
            path.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Remote path")
            });
            if path.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let target = self.path_input.clone();
                self.open(&target);
            }
        });
        ui.horizontal_wrapped(|ui| {
            if ui.button("New folder").clicked() {
                self.edit = Some(Edit::NewFolder(String::new()));
            }
            let selected = self.single_selected();
            let any_selected = !self.selected.is_empty();
            if let Some(entry) = selected.as_ref().filter(|e| e.is_dir())
                && ui.button("Open").clicked()
            {
                let path = entry.path.clone();
                self.open(&path);
            }
            if ui
                .add_enabled(any_selected, egui::Button::new("Download"))
                .on_hover_text("Download to the download folder")
                .clicked()
            {
                let dir = self.download_dir.clone();
                self.download_selected_into(&dir);
            }
            if ui
                .add_enabled(
                    any_selected && !self.picker.is_busy(),
                    egui::Button::new("Download to…"),
                )
                .on_hover_text("Choose a folder to download to")
                .clicked()
            {
                self.download_selected_to(ui.ctx());
            }
            let idle = !self.picker.is_busy();
            let start = self.download_dir.display().to_string();
            if ui
                .add_enabled(idle, egui::Button::new("Upload…"))
                .on_hover_text("Choose files to upload here")
                .clicked()
            {
                self.picker
                    .open(ui.ctx(), PickId::UploadFiles, PickKind::OpenFiles, &start);
            }
            if ui
                .add_enabled(idle, egui::Button::new("Upload folder…"))
                .on_hover_text("Choose a folder to upload here, with everything in it")
                .clicked()
            {
                self.picker
                    .open(ui.ctx(), PickId::UploadFolder, PickKind::Folder, &start);
            }
            if ui
                .add_enabled(selected.is_some(), egui::Button::new("Rename"))
                .clicked()
                && let Some(entry) = &selected
            {
                self.edit = Some(Edit::Rename {
                    from: entry.path.clone(),
                    name: entry.name.clone(),
                });
            }
            if ui
                .add_enabled(selected.is_some(), egui::Button::new("Delete"))
                .clicked()
                && let Some(entry) = selected
            {
                self.edit = Some(Edit::Delete(entry));
            }
        });
        self.edit_ui(ui);
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        ui.separator();

        let mut open = None;
        let mut download = None;
        let mut clicked = None;
        let mut menu = None;
        let recent = self
            .recent_dir
            .clone()
            .filter(|dir| *dir != self.download_dir);
        egui::ScrollArea::vertical()
            .max_height((ui.available_height() - 90.0).max(80.0))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if self.entries.is_empty() {
                    ui.weak("Empty folder");
                }
                for (index, entry) in self.entries.iter().enumerate() {
                    let icon = if entry.is_dir() { "📁" } else { "📄" };
                    let selected = self.selected.contains(&entry.path);
                    let label = if entry.is_dir() {
                        format!("{icon} {}", entry.name)
                    } else {
                        format!("{icon} {}  ({})", entry.name, format_size(entry.size))
                    };
                    let response = ui.selectable_label(selected, label);
                    if response.clicked() {
                        clicked = Some((index, ui.input(|i| i.modifiers)));
                    }
                    // Right-clicking outside the selection selects just that entry.
                    if response.secondary_clicked() && !selected {
                        clicked = Some((index, egui::Modifiers::NONE));
                    }
                    if response.double_clicked() {
                        if entry.is_dir() {
                            open = Some(entry.path.clone());
                        } else {
                            download = Some(entry.clone());
                        }
                    }
                    response.context_menu(|ui| {
                        if entry.is_dir() && ui.button("Open").clicked() {
                            menu = Some(MenuAction::Open(entry.path.clone()));
                            ui.close();
                        }
                        if ui.button("Download").clicked() {
                            menu = Some(MenuAction::Download);
                            ui.close();
                        }
                        if ui.button("Download to…").clicked() {
                            menu = Some(MenuAction::DownloadTo);
                            ui.close();
                        }
                        if let Some(dir) = &recent {
                            let name = dir.file_name().map_or_else(
                                || dir.display().to_string(),
                                |n| n.to_string_lossy().into_owned(),
                            );
                            if ui
                                .button(format!("Download to {name}"))
                                .on_hover_text(dir.display().to_string())
                                .clicked()
                            {
                                menu = Some(MenuAction::DownloadInto(dir.clone()));
                                ui.close();
                            }
                        }
                    });
                }
            });
        if let Some((index, modifiers)) = clicked {
            self.click_entry(index, modifiers);
        }
        if let Some(path) = open {
            self.open(&path);
        }
        if let Some(entry) = download {
            self.download(&entry);
        }
        match menu {
            Some(MenuAction::Open(path)) => self.open(&path),
            Some(MenuAction::Download) => {
                let dir = self.download_dir.clone();
                self.download_selected_into(&dir);
            }
            Some(MenuAction::DownloadTo) => self.download_selected_to(ui.ctx()),
            Some(MenuAction::DownloadInto(dir)) => self.download_selected_into(&dir),
            None => {}
        }

        ui.separator();
        ui.weak(format!(
            "Drop files or folders here to upload. Ctrl- or Shift-click to select several. Downloads go to {}.",
            self.download_dir.display()
        ));
        let mut cancel = None;
        for transfer in self.transfers.iter().rev().take(5) {
            ui.horizontal(|ui| {
                let arrow = match transfer.direction {
                    Direction::Download => "⬇",
                    Direction::Upload => "⬆",
                };
                ui.label(format!("{arrow} {}", transfer.name));
                match &transfer.state {
                    TransferState::Running => {
                        let fraction = if transfer.total == 0 {
                            0.0
                        } else {
                            transfer.done as f32 / transfer.total as f32
                        };
                        ui.add(
                            egui::ProgressBar::new(fraction)
                                .desired_width(100.0)
                                .show_percentage(),
                        );
                        if ui.small_button("×").on_hover_text("Cancel").clicked() {
                            cancel = Some(transfer.id);
                        }
                    }
                    TransferState::Done => {
                        ui.weak("done");
                    }
                    TransferState::Failed(e) => {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                }
            });
        }
        if let (Some(id), Some(handle)) = (cancel, &self.handle) {
            handle.cancel(id);
        }
    }

    /// The selected entries, in listing order.
    fn selected_entries(&self) -> Vec<Entry> {
        self.entries
            .iter()
            .filter(|e| self.selected.contains(&e.path))
            .cloned()
            .collect()
    }

    /// The only selected entry, if exactly one is selected.
    fn single_selected(&self) -> Option<Entry> {
        match self.selected_entries().as_slice() {
            [entry] => Some(entry.clone()),
            _ => None,
        }
    }

    fn edit_ui(&mut self, ui: &mut Ui) {
        let Some(edit) = self.edit.as_mut() else {
            return;
        };
        let mut done = false;
        let mut command = None;
        ui.horizontal(|ui| match edit {
            Edit::NewFolder(name) => {
                ui.add(
                    egui::TextEdit::singleline(name)
                        .hint_text("folder name")
                        .desired_width(140.0),
                )
                .widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Folder name")
                });
                if ui.button("Create").clicked() && !name.trim().is_empty() {
                    command = Some(FilesCommand::Mkdir(rooste_sftp::join(
                        &self.cwd,
                        name.trim(),
                    )));
                    done = true;
                }
                if ui.button("Cancel").clicked() {
                    done = true;
                }
            }
            Edit::Rename { from, name } => {
                ui.add(egui::TextEdit::singleline(name).desired_width(140.0))
                    .widget_info(|| {
                        egui::WidgetInfo::labeled(egui::Role::TextInput, true, "New name")
                    });
                if ui.button("Rename").clicked() && !name.trim().is_empty() {
                    command = Some(FilesCommand::Rename {
                        from: from.clone(),
                        to: rooste_sftp::join(&rooste_sftp::parent(from), name.trim()),
                    });
                    done = true;
                }
                if ui.button("Cancel").clicked() {
                    done = true;
                }
            }
            Edit::Delete(entry) => {
                ui.label(format!("Delete {}?", entry.name));
                if ui.button("Delete").clicked() {
                    command = Some(FilesCommand::Remove {
                        path: entry.path.clone(),
                        dir: entry.is_dir(),
                    });
                    done = true;
                }
                if ui.button("Cancel").clicked() {
                    done = true;
                }
            }
        });
        if let (Some(command), Some(handle)) = (command, &self.handle) {
            handle.send(command);
        }
        if done {
            self.edit = None;
        }
    }
}

/// `dir/name`, or `dir/name (1)` etc. if that already exists.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("some free name")
}

/// `~/Downloads`, or the home directory if that does not exist.
pub fn default_download_dir() -> PathBuf {
    let home = std::env::home_dir().unwrap_or_else(std::env::temp_dir);
    let downloads = home.join("Downloads");
    if downloads.is_dir() { downloads } else { home }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_download_names() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(unique_path(dir.path(), "a.txt"), dir.path().join("a.txt"));
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        assert_eq!(
            unique_path(dir.path(), "a.txt"),
            dir.path().join("a (1).txt")
        );
        std::fs::write(dir.path().join("a (1).txt"), "").unwrap();
        assert_eq!(
            unique_path(dir.path(), "a.txt"),
            dir.path().join("a (2).txt")
        );
        std::fs::write(dir.path().join("Makefile"), "").unwrap();
        assert_eq!(
            unique_path(dir.path(), "Makefile"),
            dir.path().join("Makefile (1)")
        );
    }
}
