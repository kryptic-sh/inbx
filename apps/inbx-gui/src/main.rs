use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use eframe::egui;
use inbx_config::Account;
use inbx_store::{FolderRow, MessageRow, Store};
use tokio::runtime::Runtime;

#[derive(Parser)]
#[command(name = "inbx-gui", version, about = "inbx GUI front-end")]
struct Cli {
    #[arg(long)]
    account: Option<String>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();

    let runtime = Arc::new(Runtime::new()?);
    let cfg = inbx_config::load()?;
    let acct = pick_account(&cfg, cli.account.as_deref())?.clone();
    let store = runtime.block_on(Store::open(&acct.name))?;
    let folders = runtime.block_on(store.list_folders())?;
    let initial_folder = folders.first().map(|f| f.name.clone());
    let messages = match initial_folder.as_deref() {
        Some(name) => runtime.block_on(store.list_messages(name, 200))?,
        None => Vec::new(),
    };

    let app = App {
        runtime,
        account: acct.clone(),
        store: Arc::new(store),
        folders,
        selected_folder: initial_folder,
        messages,
        selected_message: 0,
        body_cache: String::new(),
        composer: None,
    };

    eframe::run_native(
        "inbx",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 700.0]),
            ..Default::default()
        },
        Box::new(|_cc| Ok::<Box<dyn eframe::App>, _>(Box::new(app))),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))?;
    Ok(())
}

fn pick_account<'a>(cfg: &'a inbx_config::Config, name: Option<&str>) -> Result<&'a Account> {
    match name {
        Some(n) => cfg
            .accounts
            .iter()
            .find(|a| a.name == n)
            .with_context(|| format!("no account named {n}")),
        None => match cfg.accounts.as_slice() {
            [] => bail!("no accounts configured; run `inbx accounts add`"),
            [only] => Ok(only),
            _ => bail!("multiple accounts; pass --account NAME"),
        },
    }
}

struct App {
    runtime: Arc<Runtime>,
    account: Account,
    store: Arc<Store>,
    folders: Vec<FolderRow>,
    selected_folder: Option<String>,
    messages: Vec<MessageRow>,
    selected_message: usize,
    body_cache: String,
    composer: Option<ComposerState>,
}

#[derive(Default)]
struct ComposerState {
    subject: String,
    to: String,
    cc: String,
    bcc: String,
    body: String,
    status: String,
}

impl App {
    fn send_composer(&mut self) {
        let Some(c) = self.composer.as_mut() else {
            return;
        };
        let raw = build_mime(&self.account, &c.subject, &c.to, &c.cc, &c.bcc, &c.body);
        let acct = self.account.clone();
        let result = self
            .runtime
            .block_on(async move { inbx_net::send_message(&acct, &raw).await });
        match result {
            Ok(()) => {
                self.composer = None;
            }
            Err(e) => {
                if let Some(c) = self.composer.as_mut() {
                    c.status = format!("send failed: {e}");
                }
            }
        }
    }

    fn reload_messages(&mut self) {
        let folder = match &self.selected_folder {
            Some(f) => f.clone(),
            None => return,
        };
        let store = self.store.clone();
        let rt = self.runtime.clone();
        match rt.block_on(async move { store.list_messages(&folder, 200).await }) {
            Ok(rows) => {
                self.messages = rows;
                self.selected_message = 0;
                self.refresh_body();
            }
            Err(e) => {
                tracing::error!(%e, "list_messages failed");
                self.messages.clear();
            }
        }
    }

    fn refresh_body(&mut self) {
        let Some(msg) = self.messages.get(self.selected_message) else {
            self.body_cache.clear();
            return;
        };
        let Some(path) = msg.maildir_path.as_deref() else {
            self.body_cache = format!(
                "[body not yet fetched — run `inbx fetch --bodies` to download]\n\n\
                 from: {}\nsubject: {}",
                msg.from_addr.as_deref().unwrap_or(""),
                msg.subject.as_deref().unwrap_or(""),
            );
            return;
        };
        let raw = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                self.body_cache = format!("(unable to read {path}: {e})");
                return;
            }
        };
        let auth = inbx_render::auth::evaluate(&raw);
        let security = inbx_render::pgp::detect(&raw);
        let banner = format!(
            "[spf={:?} dkim={:?} dmarc={:?}]{}\n",
            auth.auth.spf,
            auth.auth.dkim,
            auth.auth.dmarc,
            security
                .label
                .map(|l| format!(" [{l}]"))
                .unwrap_or_default(),
        );
        match inbx_render::render_message(&raw, inbx_render::RemotePolicy::Block) {
            Ok(r) => {
                let blocked = if r.blocked_remote > 0 {
                    format!(
                        "[remote blocked: {} url(s); trackers: {}]\n",
                        r.blocked_remote,
                        r.trackers.len()
                    )
                } else {
                    String::new()
                };
                self.body_cache = format!("{banner}{blocked}\n{}", r.plain);
            }
            Err(e) => {
                self.body_cache = format!(
                    "{banner}(render error: {e})\n\n{}",
                    String::from_utf8_lossy(&raw)
                );
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut open_compose = false;
        egui::TopBottomPanel::top("hdr").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("inbx");
                ui.label(format!("· {} <{}>", self.account.name, self.account.email));
                ui.separator();
                if ui.button("✉ Compose").clicked() {
                    open_compose = true;
                }
            });
        });
        if open_compose && self.composer.is_none() {
            self.composer = Some(ComposerState::default());
        }

        if self.composer.is_some() {
            self.draw_composer(ctx);
            return;
        }

        let mut new_folder: Option<String> = None;
        egui::SidePanel::left("folders")
            .resizable(true)
            .default_width(220.0)
            .show(ctx, |ui| {
                ui.heading("folders");
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for f in &self.folders {
                        let selected = self.selected_folder.as_deref() == Some(&f.name);
                        let label = match f.special_use.as_deref() {
                            Some(s) => format!("{} {}", f.name, s),
                            None => f.name.clone(),
                        };
                        if ui.selectable_label(selected, label).clicked()
                            && self.selected_folder.as_deref() != Some(&f.name)
                        {
                            new_folder = Some(f.name.clone());
                        }
                    }
                });
            });
        if let Some(name) = new_folder {
            self.selected_folder = Some(name);
            self.reload_messages();
        }

        let mut new_msg: Option<usize> = None;
        egui::SidePanel::left("messages")
            .resizable(true)
            .default_width(380.0)
            .show(ctx, |ui| {
                ui.heading(self.selected_folder.as_deref().unwrap_or("(no folder)"));
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (i, m) in self.messages.iter().enumerate() {
                        let unread = !m.flags.to_ascii_lowercase().contains("seen");
                        let from = m
                            .from_addr
                            .as_deref()
                            .unwrap_or("")
                            .chars()
                            .take(28)
                            .collect::<String>();
                        let subj = m
                            .subject
                            .as_deref()
                            .unwrap_or("(no subject)")
                            .chars()
                            .take(70)
                            .collect::<String>();
                        let prefix = if unread { "● " } else { "  " };
                        let label = format!("{prefix}{from:<28}  {subj}");
                        let selected = i == self.selected_message;
                        if ui.selectable_label(selected, label).clicked()
                            && i != self.selected_message
                        {
                            new_msg = Some(i);
                        }
                    }
                });
            });
        if let Some(i) = new_msg {
            self.selected_message = i;
            self.refresh_body();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            let title = self
                .messages
                .get(self.selected_message)
                .and_then(|m| m.subject.clone())
                .unwrap_or_else(|| "(no message selected)".into());
            ui.heading(title);
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.body_cache.as_str())
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY),
                );
            });
        });
    }
}

impl App {
    fn draw_composer(&mut self, ctx: &egui::Context) {
        let mut send_clicked = false;
        let mut cancel_clicked = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(c) = self.composer.as_mut() else {
                return;
            };
            ui.heading("compose");
            ui.separator();
            egui::Grid::new("compose-headers")
                .num_columns(2)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Subject:");
                    ui.add(egui::TextEdit::singleline(&mut c.subject).desired_width(f32::INFINITY));
                    ui.end_row();
                    ui.label("To:");
                    ui.add(egui::TextEdit::singleline(&mut c.to).desired_width(f32::INFINITY));
                    ui.end_row();
                    ui.label("Cc:");
                    ui.add(egui::TextEdit::singleline(&mut c.cc).desired_width(f32::INFINITY));
                    ui.end_row();
                    ui.label("Bcc:");
                    ui.add(egui::TextEdit::singleline(&mut c.bcc).desired_width(f32::INFINITY));
                    ui.end_row();
                });
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut c.body)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY)
                        .desired_rows(20),
                );
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Send").clicked() {
                    send_clicked = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel_clicked = true;
                }
                if !c.status.is_empty() {
                    ui.label(c.status.as_str());
                }
            });
        });
        if cancel_clicked {
            self.composer = None;
        } else if send_clicked {
            self.send_composer();
        }
    }
}

fn build_mime(
    account: &Account,
    subject: &str,
    to: &str,
    cc: &str,
    bcc: &str,
    body: &str,
) -> Vec<u8> {
    use mail_builder::MessageBuilder;
    let parse = |s: &str| {
        s.split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .map(|addr| (String::new(), addr))
            .collect::<Vec<_>>()
    };
    let mut builder = MessageBuilder::new()
        .from((String::new(), account.email.clone()))
        .to(parse(to))
        .subject(subject)
        .text_body(body);
    let cc = parse(cc);
    if !cc.is_empty() {
        builder = builder.cc(cc);
    }
    let bcc = parse(bcc);
    if !bcc.is_empty() {
        builder = builder.bcc(bcc);
    }
    builder.write_to_vec().unwrap_or_default()
}
