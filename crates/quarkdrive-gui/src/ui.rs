//! egui drawing for the Quarkdrive desktop GUI.
//!
//! `draw` is deliberately the only entry point: it styles the context,
//! pumps finished worker events into the app state, then renders whichever
//! screen the app is on. All behaviour lives in [`crate::app::App`] so the
//! headless tests exercise exactly what a click would.

use crate::api::{human_size, human_time};
use crate::app::{App, Listing, Tab};
use egui::{Color32, RichText, ScrollArea, TextEdit};

const ACCENT: Color32 = Color32::from_rgb(0x4C, 0x8D, 0xFF);
const ERR: Color32 = Color32::from_rgb(0xE5, 0x6A, 0x6A);
const OK: Color32 = Color32::from_rgb(0x7B, 0xC8, 0x90);
const MUTED: Color32 = Color32::from_rgb(0x9A, 0x9F, 0xA8);

pub fn draw(app: &mut App, ctx: &egui::Context) {
    app.set_ctx(ctx);
    apply_style(ctx);
    app.poll();

    top_bar(app, ctx);
    status_bar(app, ctx);

    if app.api.is_some() {
        egui::CentralPanel::default().show(ctx, |ui| main_ui(app, ui));
    } else {
        egui::CentralPanel::default().show(ctx, |ui| login_ui(app, ui));
    }
}

fn apply_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = Color32::from_rgb(0x17, 0x19, 0x1E);
    style.visuals.extreme_bg_color = Color32::from_rgb(0x0F, 0x10, 0x13);
    style.visuals.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    style.visuals.selection.stroke = egui::Stroke::new(1.0_f32, ACCENT);
    style.visuals.widgets.hovered.bg_fill = ACCENT.gamma_multiply(0.25);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    ctx.set_style(style);
}

// ------------------------------------------------------------ top bar

fn top_bar(app: &mut App, ctx: &egui::Context) {
    egui::TopBottomPanel::top("topbar").show(ctx, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.strong("Quarkdrive");

            if app.api.is_some() {
                vault_picker(app, ui);

                if ui
                    .add_enabled(app.busy == 0, egui::Button::new("Refresh"))
                    .clicked()
                {
                    app.refresh();
                }

                ui.separator();
                if ui
                    .add(
                        egui::SelectableLabel::new(app.tab == Tab::Files, "Files"),
                    )
                    .clicked()
                {
                    app.tab = Tab::Files;
                }
                if ui
                    .add(
                        egui::SelectableLabel::new(app.tab == Tab::Photos, "Photos"),
                    )
                    .clicked()
                {
                    app.tab = Tab::Photos;
                    app.open_photos();
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Sign out").clicked() {
                        app.logout();
                    }
                    let resp = ui.add(
                        TextEdit::singleline(&mut app.search_buf)
                            .desired_width(170.0)
                            .hint_text("Search files…"),
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        app.do_search();
                    }
                });
            }
        });
        ui.add_space(4.0);
    });
}

fn vault_picker(app: &mut App, ui: &mut egui::Ui) {
    let mut chosen = app.vault.clone();
    egui::ComboBox::from_id_source("vault-select")
        .selected_text(app.vault.clone().unwrap_or_else(|| "choose vault".into()))
        .width(160.0)
        .show_ui(ui, |ui| {
            for v in &app.vaults {
                let label = if v.encrypted {
                    format!("{} (encrypted)", v.name)
                } else {
                    v.name.clone()
                };
                ui.selectable_value(&mut chosen, Some(v.name.clone()), label);
            }
        });
    if chosen != app.vault {
        if let Some(name) = chosen {
            app.pick_vault(&name);
        }
    }
}

// -------------------------------------------------------- status bar

fn status_bar(app: &mut App, ctx: &egui::Context) {
    egui::TopBottomPanel::bottom("statusbar").show(ctx, |ui| {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            if app.busy > 0 {
                ui.add(egui::Spinner::new().size(14.0));
            }
            match &app.note {
                Some((true, msg)) => ui.label(RichText::new(msg).color(ERR)),
                Some((false, msg)) => ui.label(RichText::new(msg).color(OK)),
                None => ui.label(RichText::new("ready").color(MUTED)),
            };
        });
        ui.add_space(2.0);
    });
    if app.busy > 0 {
        ctx.request_repaint();
    }
}

// ------------------------------------------------------------- login

fn login_ui(app: &mut App, ui: &mut egui::Ui) {
    if !app.status_checked {
        app.check_status();
    }
    ui.vertical_centered(|ui| {
        ui.add_space(48.0);
        ui.heading("Quarkdrive");
        ui.label(
            RichText::new("your files and photos, on your own server").color(MUTED),
        );
        ui.add_space(24.0);

        egui::Grid::new("login-grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                let label = |ui: &mut egui::Ui, text: &str| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(text);
                    });
                };

                label(ui, "Server");
                let before = app.server.clone();
                ui.add(
                    TextEdit::singleline(&mut app.server)
                        .desired_width(300.0)
                        .hint_text("http://localhost:8787"),
                );
                if before != app.server {
                    // Server changed — the first-run answer is stale now.
                    app.signup_hint = None;
                    app.status_checked = false;
                }
                ui.end_row();

                label(ui, "Username");
                ui.add(TextEdit::singleline(&mut app.username).desired_width(300.0));
                ui.end_row();

                label(ui, "Password");
                ui.add(
                    TextEdit::singleline(&mut app.password)
                        .password(true)
                        .desired_width(300.0),
                );
                ui.end_row();

                if app.signup_hint == Some(true) {
                    label(ui, "Vault name");
                    ui.add(
                        TextEdit::singleline(&mut app.vault_buf)
                            .desired_width(300.0)
                            .hint_text("your first vault"),
                    );
                    ui.end_row();
                }
            });

        ui.add_space(8.0);

        let (label, explain) = match app.signup_hint {
            None => ("Connect", "checking the server…"),
            Some(true) => (
                "Create account",
                "this server has no accounts yet — create the first one",
            ),
            Some(false) => ("Sign in", ""),
        };
        if ui
            .add_enabled(app.busy == 0, egui::Button::new(RichText::new(label).strong()))
            .clicked()
        {
            app.primary();
        }
        if !explain.is_empty() {
            ui.label(RichText::new(explain).small().color(MUTED));
        }

        if let Some(err) = &app.login_err {
            ui.add_space(4.0);
            ui.label(RichText::new(err).color(ERR));
        }
        ui.add_space(24.0);
        ui.label(
            RichText::new(format!("Quarkdrive {}", env!("CARGO_PKG_VERSION")))
                .small()
                .color(MUTED),
        );
    });
}

// -------------------------------------------------------------- main

fn main_ui(app: &mut App, ui: &mut egui::Ui) {
    if app.vault.is_none() {
        no_vault_ui(app, ui);
        return;
    }
    match app.tab {
        Tab::Files => files_ui(app, ui),
        Tab::Photos => photos_ui(app, ui),
    }
}

fn no_vault_ui(app: &mut App, ui: &mut egui::Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(40.0);
        ui.heading("No vaults yet");
        ui.label(RichText::new("a vault is a synced space for your files").color(MUTED));
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let mut name = app.vault_new_buf.clone();
            let resp = ui.add(TextEdit::singleline(&mut name).desired_width(240.0).hint_text("vault name"));
            app.vault_new_buf = name;
            if (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                || ui
                    .add_enabled(app.busy == 0, egui::Button::new("Create vault"))
                    .clicked()
            {
                let name = app.vault_new_buf.trim().to_string();
                app.create_vault(&name);
            }
        });
    });
}

fn files_ui(app: &mut App, ui: &mut egui::Ui) {
    breadcrumb(app, ui);
    toolbar(app, ui);

    // Pending rename, delete confirmation, new-folder row.
    if let Some((from, buf)) = app.rename.clone() {
        ui.horizontal(|ui| {
            let name = from.rsplit('/').next().unwrap_or(&from).to_string();
            ui.label(format!("Rename {name} to:"));
            let mut b = buf;
            let resp = ui.add(TextEdit::singleline(&mut b).desired_width(220.0));
            app.rename = Some((from, b));
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if enter || ui.button("Save").clicked() {
                app.commit_rename();
            } else if ui.button("Cancel").clicked() {
                app.rename = None;
            }
        });
    }
    if let Some(path) = app.delete_arm.clone() {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Delete {path}?")).color(ERR));
            if ui.add_enabled(app.busy == 0, egui::Button::new("Yes, delete")).clicked() {
                app.confirm_delete();
            }
            if ui.button("Keep it").clicked() {
                app.delete_arm = None;
            }
        });
    }
    if app.mkdir_open {
        ui.horizontal(|ui| {
            ui.label("New folder:");
            let resp = ui.add(TextEdit::singleline(&mut app.mkdir_buf).desired_width(220.0));
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if enter || ui.button("Create").clicked() {
                app.do_mkdir();
            } else if ui.button("Cancel").clicked() {
                app.mkdir_open = false;
            }
        });
    }

    ui.separator();

    if let Listing::Search(q) = app.listing.clone() {
        ui.horizontal(|ui| {
            ui.label(format!("Search results for “{q}”"));
            if ui.button("Back to files").clicked() {
                app.close_search();
            }
        });
    }

    // Interactions are collected while drawing and applied afterwards, so
    // the grid closure never holds a mutable borrow across an action.
    let mut select: Option<String> = None;
    let mut open: Option<String> = None;
    let mut grab: Option<String> = None;
    let mut edit: Option<String> = None;
    let mut arm: Option<String> = None;

    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let mut sorted = app.entries.clone();
        sorted.sort_by(|a, b| {
            b.is_dir()
                .cmp(&a.is_dir())
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });

        egui::Grid::new("files-grid")
            .num_columns(4)
            .min_col_width(60.0)
            .spacing([16.0, 3.0])
            .show(ui, |ui| {
                for e in &sorted {
                    let selected = app.selected.as_deref() == Some(e.path.as_str());
                    let icon = if e.is_dir() { "▸" } else { " " };
                    let resp = ui.add_sized(
                        [ui.available_width().max(200.0) - 430.0, 18.0],
                        egui::SelectableLabel::new(
                            selected,
                            RichText::new(format!("{icon} {}", e.name)).color(if e.is_dir() {
                                ACCENT
                            } else {
                                Color32::WHITE
                            }),
                        ),
                    );
                    if resp.clicked() {
                        select = Some(e.path.clone());
                    }
                    if resp.double_clicked() {
                        if e.is_dir() {
                            open = Some(e.path.clone());
                        } else {
                            grab = Some(e.path.clone());
                        }
                    }
                    ui.label(RichText::new(if e.is_dir() { "—".into() } else { human_size(e.size) }).color(MUTED));
                    ui.label(RichText::new(human_time(e.mtime)).color(MUTED));
                    ui.horizontal(|ui| {
                        if !e.is_dir()
                            && ui
                                .add_enabled(app.busy == 0, egui::Button::new("get"))
                                .clicked()
                        {
                            grab = Some(e.path.clone());
                        }
                        if ui
                            .add_enabled(app.busy == 0, egui::Button::new("rename"))
                            .clicked()
                        {
                            edit = Some(e.path.clone());
                        }
                        if ui
                            .add_enabled(app.busy == 0, egui::Button::new("delete"))
                            .clicked()
                        {
                            arm = Some(e.path.clone());
                        }
                    });
                    ui.end_row();
                }
            });
    });

    if let Some(p) = select {
        app.select(&p);
    }
    if let Some(p) = open {
        app.open_dir(&p);
    }
    if let Some(p) = grab {
        app.download(&p);
    }
    if let Some(p) = edit {
        app.start_rename(&p);
    }
    if let Some(p) = arm {
        app.delete_arm = Some(p);
    }

    upload_row(app, ui);
}

fn breadcrumb(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        let vault = app.vault.clone().unwrap_or_default();
        if ui.selectable_label(app.cwd.is_empty(), vault).clicked() && !app.cwd.is_empty() {
            app.open_dir("");
        }
        let cwd = app.cwd.clone();
        let mut walked = String::new();
        for seg in cwd.split('/').filter(|s| !s.is_empty()) {
            walked = if walked.is_empty() {
                seg.to_string()
            } else {
                format!("{walked}/{seg}")
            };
            ui.label(RichText::new("/").color(MUTED));
            let here = walked == app.cwd;
            if ui.selectable_label(here, seg).clicked() && !here {
                app.open_dir(&walked);
            }
        }
        if !app.cwd.is_empty() && ui.button("Up").clicked() {
            app.up();
        }
    });
}

fn toolbar(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        let enabled = app.busy == 0;
        if ui.add_enabled(enabled, egui::Button::new("New folder")).clicked() {
            app.mkdir_open = true;
        }
        if ui.add_enabled(enabled, egui::Button::new("Upload…")).clicked() {
            app.upload_via_picker();
        }
        if ui
            .add_enabled(enabled && app.selected.is_some(), egui::Button::new("Download"))
            .clicked()
        {
            app.download_selected();
        }
        if ui
            .add_enabled(enabled && app.selected.is_some(), egui::Button::new("Delete"))
            .clicked()
        {
            if let Some(p) = app.selected.clone() {
                app.delete_arm = Some(p);
            }
        }
    });
}

fn upload_row(app: &mut App, ui: &mut egui::Ui) {
    ui.separator();
    ui.horizontal(|ui| {
        ui.label(RichText::new("Upload a file:").color(MUTED));
        ui.add(
            TextEdit::singleline(&mut app.upload_buf)
                .desired_width(360.0)
                .hint_text("/path/to/file on this computer"),
        );
        if ui
            .add_enabled(app.busy == 0, egui::Button::new("Upload"))
            .clicked()
        {
            app.upload_manual();
        }
    });
}

fn photos_ui(app: &mut App, ui: &mut egui::Ui) {
    // Full-size preview overlay.
    if let Some((path, tex)) = app.preview.clone() {
        ui.horizontal(|ui| {
            if ui.button("← Back to photos").clicked() {
                app.close_preview();
            }
            ui.label(RichText::new(&path).color(MUTED));
        });
        ui.separator();
        ScrollArea::both().show(ui, |ui| {
            let size = tex.size_vec2();
            let max = ui.available_size();
            let scale = (max.x / size.x).min(max.y / size.y).min(1.0).max(0.05);
            let shown = size * scale;
            ui.add(egui::Image::new(egui::load::SizedTexture::new(&tex, shown)));
        });
        return;
    }

    let items: Vec<crate::api::Photo> = match &app.photos {
        None => {
            ui.add_space(30.0);
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                ui.label("loading photos…");
            });
            return;
        }
        Some(Err(e)) => {
            ui.label(RichText::new(format!("cannot load photos: {e}")).color(ERR));
            return;
        }
        Some(Ok(items)) => items.clone(),
    };

    if items.is_empty() {
        ui.label(RichText::new("no photos in this vault yet").color(MUTED));
        return;
    }

    let cols = ((ui.available_width() / 176.0).floor() as usize).max(1);
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("photos-grid")
            .num_columns(cols)
            .min_col_width(168.0)
            .spacing([8.0, 8.0])
            .show(ui, |ui| {
                for (i, p) in items.iter().enumerate() {
                    if i > 0 && i % cols == 0 {
                        ui.end_row();
                    }
                    photo_cell(app, ui, p);
                }
                if items.len() % cols != 0 {
                    for _ in items.len() % cols..cols {
                        ui.label("");
                    }
                    ui.end_row();
                }
            });
    });
}

fn photo_cell(app: &mut App, ui: &mut egui::Ui, p: &crate::api::Photo) {
    app.request_thumb(&p.path);
    let name = p.path.rsplit('/').next().unwrap_or(&p.path).to_string();
    let taken = p.taken_at.map(human_time);
    // One grid cell holds image + caption so rows stay aligned.
    ui.vertical(|ui| {
        let resp = match app.thumbs.get(&p.path) {
            Some(tex) => ui.add(
                egui::ImageButton::new(egui::load::SizedTexture::new(
                    tex,
                    egui::vec2(160.0, 160.0),
                )),
            ),
            None => ui.add_sized(
                [160.0, 160.0],
                egui::Button::new(RichText::new("…").color(MUTED)),
            ),
        };
        if resp.clicked() && app.busy == 0 {
            app.open_preview(&p.path);
        }
        ui.label(RichText::new(name).small());
        if let Some(t) = taken {
            ui.label(RichText::new(t).small().color(MUTED));
        }
    });
}
