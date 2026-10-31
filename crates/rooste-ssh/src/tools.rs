//! The Tools menu: key generator, PuTTY key converter and the built-in
//! SSH agent.

use std::path::PathBuf;

use egui::{Id, Modal, RichText, Ui};
use rooste_keys::{Agent, KeyType, PublicKey, ppk};

use crate::dialogs::Decision;
use crate::picker::{PickId, PickKind, Picker, path_row};

fn field(ui: &mut Ui, label: &str, value: &mut String, password: bool) -> egui::Response {
    let label = ui.label(label);
    let response = ui
        .add(
            egui::TextEdit::singleline(value)
                .margin(crate::theme::FIELD_MARGIN)
                .password(password)
                .desired_width(crate::theme::FIELD_WIDTH),
        )
        .labelled_by(label.id);
    ui.end_row();
    response
}

fn ssh_dir() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".ssh")
}

/// The key generator dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyGenerator {
    pub kind: KeyType,
    pub comment: String,
    pub passphrase: String,
    pub confirm: String,
    pub path: String,
    /// Public key line and where the key was saved.
    pub result: Option<(String, String)>,
    pub error: Option<String>,
}

impl KeyGenerator {
    pub fn new() -> Self {
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "me".into());
        let host = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "rooste".into());
        Self {
            kind: KeyType::Ed25519,
            comment: format!("{user}@{host}"),
            passphrase: String::new(),
            confirm: String::new(),
            path: ssh_dir()
                .join(KeyType::Ed25519.file_name())
                .display()
                .to_string(),
            result: None,
            error: None,
        }
    }

    fn generate(&mut self) {
        self.error = None;
        if self.passphrase != self.confirm {
            self.error = Some("The passphrases do not match.".into());
            return;
        }
        let path = rooste_core::expand_tilde(self.path.trim());
        let result = rooste_keys::generate(self.kind, &self.comment).and_then(|key| {
            let passphrase = (!self.passphrase.is_empty()).then_some(self.passphrase.as_str());
            rooste_keys::save_openssh(&key, &path, passphrase)?;
            Ok(rooste_keys::public_line(key.public_key()))
        });
        match result {
            Ok(line) => self.result = Some((line, path.display().to_string())),
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, picker: &mut Picker) -> Decision<()> {
        let mut decision = Decision::Pending;
        Modal::new(Id::unique("key-generator"))
            .frame(crate::theme::dialog_frame(ctx))
            .show(ctx, |ui| {
                ui.heading("Key generator");
                ui.add_space(10.0);
                if let Some((line, path)) = &self.result {
                    ui.label(format!("Saved to {path} and {path}.pub."));
                    ui.label("Add this public key to ~/.ssh/authorized_keys on your servers:");
                    let mut text = line.clone();
                    ui.add(
                        egui::TextEdit::multiline(&mut text)
                            .margin(crate::theme::FIELD_MARGIN)
                            .desired_rows(3)
                            .desired_width(420.0),
                    );
                    crate::dialogs::footer(ui, |ui| {
                        if ui.button("Close").clicked() {
                            decision = Decision::Accept(());
                        }
                        if ui.button("Copy public key").clicked() {
                            ui.ctx().copy_text(line.clone());
                        }
                    });
                    return;
                }
                egui::Grid::new("generation")
                    .num_columns(2)
                    .spacing(crate::theme::FORM_SPACING)
                    .show(ui, |ui| {
                        ui.label("Type");
                        let before = self.kind;
                        egui::ComboBox::from_id_salt("key-type")
                            .selected_text(self.kind.label())
                            .show_ui(ui, |ui| {
                                for kind in KeyType::ALL {
                                    ui.selectable_value(&mut self.kind, kind, kind.label());
                                }
                            });
                        if self.kind != before {
                            self.path = ssh_dir().join(self.kind.file_name()).display().to_string();
                        }
                        ui.end_row();
                        field(ui, "Comment", &mut self.comment, false);
                        field(ui, "Passphrase", &mut self.passphrase, true);
                        field(ui, "Repeat passphrase", &mut self.confirm, true);
                        path_row(
                            ui,
                            "Save as",
                            &mut self.path,
                            picker,
                            PickId::KeyGenSave,
                            PickKind::SaveFile,
                        );
                    });
                if self.passphrase.is_empty() {
                    ui.weak("Without a passphrase, anyone who gets the file can use the key.");
                }
                if let Some(error) = &self.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                crate::dialogs::footer(ui, |ui| {
                    if ui.button("Cancel").clicked() {
                        decision = Decision::Cancel;
                    }
                    if crate::theme::primary_button(ui, "Generate").clicked() {
                        self.generate();
                    }
                });
            });
        decision
    }
}

impl Default for KeyGenerator {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert between PuTTY (`.ppk`) and OpenSSH key files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyConverter {
    pub input: String,
    pub passphrase: String,
    /// Passphrase for the converted file; empty leaves it unencrypted.
    pub new_passphrase: String,
    pub output: String,
    pub done: Option<String>,
    pub error: Option<String>,
}

impl KeyConverter {
    fn convert(&mut self) {
        self.error = None;
        let input = rooste_core::expand_tilde(self.input.trim());
        let output = rooste_core::expand_tilde(self.output.trim());
        let result = (|| -> Result<String, String> {
            let text =
                std::fs::read_to_string(&input).map_err(|e| format!("{}: {e}", input.display()))?;
            let passphrase = (!self.passphrase.is_empty()).then_some(self.passphrase.as_str());
            let key = rooste_keys::load(&text, passphrase).map_err(|e| e.to_string())?;
            let new_passphrase =
                (!self.new_passphrase.is_empty()).then_some(self.new_passphrase.as_str());
            if text.trim_start().starts_with("PuTTY-User-Key-File-") {
                rooste_keys::save_openssh(&key, &output, new_passphrase)
                    .map_err(|e| e.to_string())?;
                Ok(format!("Saved an OpenSSH key to {}.", output.display()))
            } else {
                let converted = ppk::export(&key, new_passphrase).map_err(|e| e.to_string())?;
                rooste_keys::write_private(&output, converted.as_bytes())
                    .map_err(|e| format!("{}: {e}", output.display()))?;
                Ok(format!("Saved a PuTTY key to {}.", output.display()))
            }
        })();
        match result {
            Ok(message) => self.done = Some(message),
            Err(e) => self.error = Some(e),
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, picker: &mut Picker) -> Decision<()> {
        let mut decision = Decision::Pending;
        Modal::new(Id::unique("key-converter"))
            .frame(crate::theme::dialog_frame(ctx))
            .show(ctx, |ui| {
                ui.heading("Convert key");
                ui.add_space(10.0);
                ui.label(
                    "PuTTY keys (.ppk) become OpenSSH keys, and OpenSSH keys become .ppk files.",
                );
                egui::Grid::new("convert")
                    .num_columns(2)
                    .spacing(crate::theme::FORM_SPACING)
                    .show(ui, |ui| {
                        let input = path_row(
                            ui,
                            "Key file",
                            &mut self.input,
                            picker,
                            PickId::ConvertInput,
                            PickKind::OpenFile,
                        );
                        if input.changed() && self.output.is_empty() {
                            // Suggest an output name next to the input.
                        }
                        field(ui, "Passphrase", &mut self.passphrase, true);
                        path_row(
                            ui,
                            "Save as",
                            &mut self.output,
                            picker,
                            PickId::ConvertOutput,
                            PickKind::SaveFile,
                        );
                        field(ui, "New passphrase", &mut self.new_passphrase, true);
                    });
                if self.output.trim().is_empty() && !self.input.trim().is_empty() {
                    let input = self.input.trim();
                    self.output = match input.strip_suffix(".ppk") {
                        Some(stem) => stem.to_owned(),
                        None => format!("{input}.ppk"),
                    };
                }
                if let Some(done) = &self.done {
                    ui.label(done);
                }
                if let Some(error) = &self.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                crate::dialogs::footer(ui, |ui| {
                    if ui.button("Close").clicked() {
                        decision = Decision::Cancel;
                    }
                    if crate::theme::primary_button(ui, "Convert").clicked() {
                        self.convert();
                    }
                });
            });
        decision
    }
}

/// The built-in agent window.
#[derive(Debug, Default)]
pub struct AgentWindow {
    pub key_file: String,
    pub passphrase: String,
    pub keys: Vec<PublicKey>,
    pub error: Option<String>,
}

/// What the agent window asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentAction {
    Start,
    Stop,
    Load,
    Remove(String),
    Refresh,
}

impl AgentWindow {
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        agent: Option<&Agent>,
        use_for_sessions: &mut bool,
        picker: &mut Picker,
    ) -> (Decision<()>, Option<AgentAction>) {
        let mut decision = Decision::Pending;
        let mut action = None;
        Modal::new(Id::unique("agent")).frame(crate::theme::dialog_frame(ctx)).show(ctx, |ui| {
ui.heading("SSH agent");
ui.add_space(10.0);
            match agent {
                None => {
                    ui.label("The built-in agent holds your keys in memory and offers them when you log in.");
                    if crate::theme::primary_button(ui, "Start agent").clicked() {
                        action = Some(AgentAction::Start);
                    }
                }
                Some(agent) => {
                    ui.label("Running. Other programs can use it with:");
                    let var = agent.path().display().to_string();
                    #[cfg(unix)]
                    let hint = format!("export SSH_AUTH_SOCK={var}");
                    #[cfg(windows)]
                    let hint = format!("set SSH_AUTH_SOCK={var}");
                    ui.monospace(&hint);
                    ui.checkbox(use_for_sessions, "Use this agent for Rooste SSH sessions");
                    ui.separator();
                    if self.keys.is_empty() {
                        ui.weak("No keys loaded.");
                    }
                    for key in &self.keys {
                        ui.horizontal(|ui| {
                            ui.label(format!("{} {}", key.algorithm(), key.comment().as_str_lossy()));
                            ui.weak(rooste_keys::fingerprint(key));
                            if ui.small_button("Remove").clicked() {
                                action = Some(AgentAction::Remove(rooste_keys::fingerprint(key)));
                            }
                        });
                    }
                    ui.separator();
                    egui::Grid::new("agent-load").num_columns(2).spacing(crate::theme::FORM_SPACING).show(ui, |ui| {
                        path_row(ui, "Key file", &mut self.key_file, picker, PickId::AgentKeyFile, PickKind::OpenFile);
                        field(ui, "Passphrase", &mut self.passphrase, true);
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Load key").clicked() {
                            action = Some(AgentAction::Load);
                        }
                        if ui.button("Stop agent").clicked() {
                            action = Some(AgentAction::Stop);
                        }
                    });
                }
            }
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
ui.add_space(6.0);
ui.label(RichText::new("Keys are kept only while Rooste SSH runs.").small().weak());
crate::dialogs::footer(ui, |ui| {
    if ui.button("Close").clicked() {
        decision = Decision::Cancel;
    }
});
        });
        (decision, action)
    }
}
