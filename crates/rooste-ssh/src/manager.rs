//! The session manager: saved profiles in a searchable folder tree.

use std::collections::BTreeMap;

use egui::{RichText, Ui};
use rooste_core::Profile;

/// What the user asked for in the session manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerAction {
    New,
    /// A new session in this folder (`Work/Prod`).
    NewInFolder(String),
    QuickConnect,
    /// Import from PuTTY (true) or from `~/.ssh/config` (false).
    Import {
        putty: bool,
    },
    Connect(String),
    Edit(String),
    Duplicate(String),
    Delete(String),
}

#[derive(Debug, Default)]
pub struct ManagerState {
    pub search: String,
    pub selected: Option<String>,
}

#[derive(Default)]
struct Folder<'a> {
    folders: BTreeMap<String, Folder<'a>>,
    profiles: Vec<&'a Profile>,
}

impl<'a> Folder<'a> {
    fn build(profiles: &'a [Profile]) -> Self {
        let mut root = Folder::default();
        for profile in profiles {
            let mut node = &mut root;
            for part in profile.folder.split('/').filter(|s| !s.is_empty()) {
                node = node.folders.entry(part.to_owned()).or_default();
            }
            node.profiles.push(profile);
        }
        root.sort();
        root
    }

    fn sort(&mut self) {
        self.profiles
            .sort_by_key(|p| p.display_name().to_lowercase());
        for folder in self.folders.values_mut() {
            folder.sort();
        }
    }
}

pub fn show(ui: &mut Ui, state: &mut ManagerState, profiles: &[Profile]) -> Option<ManagerAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        ui.heading("Sessions");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("➕").on_hover_text("New session").clicked() {
                action = Some(ManagerAction::New);
            }
        });
    });
    let search = ui.add(
        egui::TextEdit::singleline(&mut state.search)
            .margin(crate::theme::FIELD_MARGIN)
            .hint_text("Search")
            .desired_width(f32::INFINITY),
    );
    search.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Search"));
    ui.separator();

    if profiles.is_empty() {
        ui.label(RichText::new("No saved sessions yet.").weak());
        ui.label(
            RichText::new(
                "Create one with ➕ or by right-clicking here, or import from PuTTY or \
                 ~/.ssh/config.",
            )
            .weak(),
        );
        background_menu(ui, &mut action);
        return action;
    }

    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            if state.search.trim().is_empty() {
                let root = Folder::build(profiles);
                folder_ui(ui, &root, "", state, &mut action);
            } else {
                let mut matches: Vec<&Profile> = profiles
                    .iter()
                    .filter(|p| p.matches(&state.search))
                    .collect();
                matches.sort_by_key(|p| p.display_name().to_lowercase());
                if matches.is_empty() {
                    ui.label(RichText::new("No matching sessions.").weak());
                }
                for profile in matches {
                    profile_ui(ui, profile, true, state, &mut action);
                }
            }
            background_menu(ui, &mut action);
        });
    action
}

/// Right-clicking the empty part of the panel.
fn background_menu(ui: &mut Ui, action: &mut Option<ManagerAction>) {
    let size = ui.available_size().max(egui::vec2(0.0, 40.0));
    let response = ui.allocate_response(size, egui::Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::List, true, "Session list"));
    response.context_menu(|ui| {
        let mut item = |ui: &mut Ui, label: &str, chosen: ManagerAction| {
            if ui.button(label).clicked() {
                *action = Some(chosen);
                ui.close();
            }
        };
        item(ui, "New session…", ManagerAction::New);
        item(ui, "Quick connect…", ManagerAction::QuickConnect);
        ui.separator();
        item(
            ui,
            "Import from ~/.ssh/config",
            ManagerAction::Import { putty: false },
        );
        item(
            ui,
            "Import from PuTTY",
            ManagerAction::Import { putty: true },
        );
    });
}

fn folder_ui(
    ui: &mut Ui,
    folder: &Folder<'_>,
    path: &str,
    state: &mut ManagerState,
    action: &mut Option<ManagerAction>,
) {
    for (name, child) in &folder.folders {
        let child_path = if path.is_empty() {
            name.clone()
        } else {
            format!("{path}/{name}")
        };
        let shown = egui::CollapsingHeader::new(format!("📁 {name}"))
            .id_salt(&child_path)
            .default_open(true)
            .show(ui, |ui| folder_ui(ui, child, &child_path, state, action));
        shown.header_response.context_menu(|ui| {
            if ui.button("New session in this folder…").clicked() {
                *action = Some(ManagerAction::NewInFolder(child_path.clone()));
                ui.close();
            }
        });
    }
    for profile in &folder.profiles {
        profile_ui(ui, profile, false, state, action);
    }
}

fn profile_ui(
    ui: &mut Ui,
    profile: &Profile,
    show_folder: bool,
    state: &mut ManagerState,
    action: &mut Option<ManagerAction>,
) {
    let selected = state.selected.as_deref() == Some(profile.id.as_str());
    let response = ui
        .selectable_label(selected, profile.display_name())
        .on_hover_text(format!(
            "{}{}:{}{}",
            if profile.user.is_empty() {
                String::new()
            } else {
                format!("{}@", profile.user)
            },
            profile.host,
            profile.effective_port(),
            if show_folder && !profile.folder.is_empty() {
                format!("\nin {}", profile.folder)
            } else {
                String::new()
            }
        ));
    if response.clicked() {
        state.selected = Some(profile.id.clone());
    }
    if response.double_clicked() {
        *action = Some(ManagerAction::Connect(profile.id.clone()));
    }
    response.context_menu(|ui| {
        let id = profile.id.clone();
        if ui.button("Connect").clicked() {
            *action = Some(ManagerAction::Connect(id.clone()));
        }
        if ui.button("Edit…").clicked() {
            *action = Some(ManagerAction::Edit(id.clone()));
        }
        if ui.button("Duplicate").clicked() {
            *action = Some(ManagerAction::Duplicate(id.clone()));
        }
        if ui.button("Delete…").clicked() {
            *action = Some(ManagerAction::Delete(id));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_tree_is_nested_and_sorted() {
        let p = |name: &str, folder: &str| Profile {
            name: name.into(),
            folder: folder.into(),
            host: "h".into(),
            ..Profile::default()
        };
        let profiles = vec![
            p("b", "Work/Prod"),
            p("a", "Work/Prod"),
            p("z", ""),
            p("c", "Work"),
        ];
        let root = Folder::build(&profiles);
        assert_eq!(root.profiles.len(), 1);
        let work = &root.folders["Work"];
        assert_eq!(work.profiles[0].name, "c");
        let names: Vec<_> = work.folders["Prod"]
            .profiles
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["a", "b"]);
    }
}
