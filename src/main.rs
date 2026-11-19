mod cli;
mod media;
mod merge;
mod scan;
mod sync;
mod tree;
mod undo;

use eframe::egui::{self, Color32, RichText};
use merge::{Chunk, Merge, Pick};
use scan::*;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, atomic::Ordering, mpsc};
use undo::{Journal, new_batch};

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        std::process::exit(cli::run(&args));
    }
    eframe::run_native(
        "TidyTwin",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 820.0]),
            ..Default::default()
        },
        Box::new(|_| {
            let mut app = App::default();
            let days = app.journal.settings.retention_days;
            match app.journal.purge(days) {
                Ok(0) => {}
                Ok(n) => app.log.push(format!("purged {n} quarantined files older than {days} days")),
                Err(e) => app.log.push(format!("purge: {e}")),
            }
            Ok(Box::new(app))
        }),
    )
}

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Dirs,
    System,
    TwoDirs,
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum Tab {
    Dups,
    Folders,
    Images,
    Audio,
    Largest,
    Sync,
    Merge,
    History,
}

#[derive(Default)]
struct ScanResult {
    roots: Vec<PathBuf>,
    groups: Vec<Group>,
    dirs: Vec<Group>,
    images: Vec<Group>,
    audio: Vec<Group>,
    largest: Vec<FileEntry>,
    tree: Option<tree::Tree>,
    sync: Vec<sync::Row>,
    scanned: usize,
    secs: f32,
}

struct MergeState {
    left: PathBuf,
    right: PathBuf,
    base: Option<PathBuf>,
    m: Merge,
    edited: Option<String>, // Some = user is hand-editing result
}

struct App {
    mode: Mode,
    roots: Vec<PathBuf>,
    crit: Criteria,
    min_kib: u64,
    exclude: String,
    hidden: bool,
    top_n: usize,
    find_dirs: bool,
    find_images: bool,
    img_dist: u32,
    find_audio: bool,
    audio_tol: u64,
    sched_freq: &'static str,
    tab: Tab,
    running: Option<(Arc<Progress>, mpsc::Receiver<ScanResult>)>,
    res: Option<ScanResult>,
    sel: HashSet<PathBuf>,
    filter: String,
    keep: KeepRule,
    action: Action,
    confirm: Option<Tab>,
    log: Vec<String>,
    merge: Option<MergeState>,
    journal: Journal,
    cache: Arc<HashCache>,
    confirm_purge: bool,
    treemap: bool,
    tm_node: usize,
    sync_show: [bool; 6],
    sync_sel: HashSet<PathBuf>,
    sort: Sort,
    collapsed: HashSet<(u8, usize)>,
    // ponytail: unbounded thumbnail cache; add LRU if memory matters on huge image sets.
    thumbs: HashMap<PathBuf, Option<egui::TextureHandle>>,
    thumb_ch: (mpsc::Sender<(PathBuf, egui::ColorImage)>, mpsc::Receiver<(PathBuf, egui::ColorImage)>),
    log_open: bool,
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum Sort {
    Wasted,
    Size,
    Count,
    Path,
}

/// One line of the virtualized groups list.
enum Row {
    Head(usize),
    File(usize, usize),
}

const THUMB: f32 = 72.0;

impl Default for App {
    fn default() -> Self {
        Self {
            mode: Mode::Dirs,
            roots: vec![],
            crit: Criteria::default(),
            min_kib: 1,
            exclude: SYSTEM_EXCLUDES.join(";") + ";/.git/;node_modules",
            hidden: false,
            top_n: 200,
            find_dirs: true,
            find_images: false,
            img_dist: 6,
            find_audio: false,
            audio_tol: 3,
            sched_freq: "daily",
            tab: Tab::Dups,
            running: None,
            res: None,
            sel: HashSet::new(),
            filter: String::new(),
            keep: KeepRule::Oldest,
            action: Action::Delete,
            confirm: None,
            log: vec![],
            merge: None,
            journal: Journal::open(undo::data_dir()),
            cache: Arc::new(HashCache::load(undo::data_dir().join("hashcache.jsonl"))),
            confirm_purge: false,
            treemap: true,
            tm_node: 0,
            sync_show: [true, true, false, true, true, true],
            sync_sel: HashSet::new(),
            sort: Sort::Wasted,
            collapsed: HashSet::new(),
            thumbs: HashMap::new(),
            thumb_ch: mpsc::channel(),
            log_open: false,
        }
    }
}

const SYNC_STATES: [sync::State; 6] = [sync::State::OnlyA, sync::State::OnlyB, sync::State::Same, sync::State::NewerA, sync::State::NewerB, sync::State::Differ];

impl App {
    fn scan_opts(&self) -> ScanOpts {
        let roots = match self.mode {
            Mode::System => vec![PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" })],
            _ => self.roots.clone(),
        };
        ScanOpts {
            roots,
            min_size: self.min_kib * 1024,
            exclude: self.exclude.split(';').map(|s| s.trim().to_string()).collect(),
            follow_hidden: self.hidden,
        }
    }

    fn start_scan(&mut self) {
        let opts = self.scan_opts();
        let (crit, two, top_n) = (self.crit, self.mode == Mode::TwoDirs, self.top_n);
        let (fd, fi, fa, dist, tol) = (self.find_dirs, self.find_images, self.find_audio, self.img_dist, self.audio_tol);
        let prog = Arc::new(Progress::default());
        let (p, cache) = (prog.clone(), self.cache.clone());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let t = std::time::Instant::now();
            let files = collect(&opts, &p);
            let roots = opts.roots;
            if !p.cancel.load(Ordering::Relaxed) {
                cache.prune(&roots, &files);
            }
            let mut r = ScanResult {
                largest: largest(&files, top_n),
                tree: Some(tree::Tree::build(&files, &roots)),
                scanned: files.len(),
                sync: if two { sync::compare(&files, &roots, crit.content, &cache) } else { vec![] },
                ..Default::default()
            };
            if fd {
                p.set_phase(1);
                r.dirs = tree::dup_dirs(&files, &roots, &p, &cache);
            }
            if fi {
                p.set_phase(2);
                r.images = media::similar_images(&files, dist, &p);
            }
            if fa {
                p.set_phase(3);
                r.audio = media::similar_audio(&files, tol, &p);
            }
            p.set_phase(4);
            r.groups = find_dups(files, crit, two, &p, &cache);
            r.roots = roots;
            r.secs = t.elapsed().as_secs_f32();
            let _ = cache.save();
            let _ = tx.send(r);
        });
        self.running = Some((prog, rx));
        self.sel.clear();
        self.sync_sel.clear();
        self.tm_node = 0;
        self.collapsed.clear();
        self.thumbs.clear();
    }

    /// Visible text color for warnings in the current theme.
    fn warn(ui: &egui::Ui) -> Color32 {
        ui.visuals().warn_fg_color
    }
    fn err(ui: &egui::Ui) -> Color32 {
        ui.visuals().error_fg_color
    }

    /// Right-click menu for a file path. Returns a folder the user asked to protect.
    fn file_menu(resp: &egui::Response, path: &std::path::Path) -> Option<PathBuf> {
        let mut protect = None;
        resp.context_menu(|ui| {
            if ui.button("Open file").clicked() {
                let _ = open::that(path);
                ui.close();
            }
            if ui.button("Open containing folder").clicked() {
                let _ = open::that(path.parent().unwrap_or(path));
                ui.close();
            }
            if ui.button("Copy path").clicked() {
                ui.ctx().copy_text(path.display().to_string());
                ui.close();
            }
            if let Some(dir) = path.parent()
                && ui.button("🔒 Protect this folder").clicked()
            {
                protect = Some(dir.to_path_buf());
                ui.close();
            }
        });
        protect
    }

    fn protect(&mut self, dir: PathBuf) {
        if !self.journal.settings.protected.contains(&dir) {
            self.log.push(format!("protected {}", dir.display()));
            self.journal.settings.protected.push(dir);
            self.save_settings();
        }
    }

    /// Thumbnail for an image row; decodes off the UI thread on first request.
    fn thumb(thumbs: &mut HashMap<PathBuf, Option<egui::TextureHandle>>, tx: &mpsc::Sender<(PathBuf, egui::ColorImage)>, ui: &mut egui::Ui, path: &std::path::Path) {
        match thumbs.get(path) {
            Some(Some(t)) => {
                let s = t.size_vec2();
                let k = THUMB / s.x.max(s.y);
                ui.add_sized([THUMB, THUMB], egui::Image::new((t.id(), s * k)));
            }
            Some(None) => {
                ui.add_sized([THUMB, THUMB], egui::Spinner::new());
            }
            None => {
                thumbs.insert(path.to_path_buf(), None);
                let (tx, p, ctx) = (tx.clone(), path.to_path_buf(), ui.ctx().clone());
                rayon::spawn(move || {
                    // Undecodable file → blank 1×1, so it isn't retried every frame.
                    let img = image::open(&p)
                        .map(|i| {
                            let t = i.thumbnail(THUMB as u32 * 2, THUMB as u32 * 2).to_rgba8();
                            egui::ColorImage::from_rgba_unmultiplied([t.width() as usize, t.height() as usize], t.as_raw())
                        })
                        .unwrap_or_else(|_| egui::ColorImage::new([1, 1], vec![Color32::TRANSPARENT]));
                    let _ = tx.send((p, img));
                    ctx.request_repaint();
                });
                ui.add_sized([THUMB, THUMB], egui::Spinner::new());
            }
        }
    }

    fn open_merge(&mut self, l: PathBuf, r: PathBuf, base: Option<PathBuf>) {
        let read = |p: &PathBuf| std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()));
        let res = (|| {
            let (a, b) = (read(&l)?, read(&r)?);
            Ok::<_, String>(match &base {
                Some(bp) => Merge::new3(&read(bp)?, &a, &b),
                None => Merge::new(&a, &b),
            })
        })();
        match res {
            Ok(m) => {
                self.merge = Some(MergeState { m, left: l, right: r, base, edited: None });
                self.tab = Tab::Merge;
            }
            Err(e) => self.log.push(format!("open failed: {e}")),
        }
    }

    /// Folders can't be permanently deleted (no rebuild path), so Delete quarantines them.
    fn effective_action(&self, tab: Tab) -> Action {
        if self.action == Action::Delete && (self.journal.settings.quarantine || tab == Tab::Folders) { Action::Quarantine } else { self.action }
    }

    fn save_settings(&mut self) {
        if let Err(e) = self.journal.save() {
            self.log.push(format!("settings: {e}"));
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Scan");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), egui::widgets::global_theme_preference_buttons);
        });
        egui::CollapsingHeader::new("Where").default_open(true).show(ui, |ui| self.sidebar_where(ui));
        egui::CollapsingHeader::new("What to find").default_open(true).show(ui, |ui| self.sidebar_what(ui));
        ui.separator();
        let any_crit = self.crit.name || self.crit.size || self.crit.mtime || self.crit.content;
        let ready = any_crit
            && match self.mode {
                Mode::System => true,
                Mode::Dirs => !self.roots.is_empty(),
                Mode::TwoDirs => self.roots.len() == 2,
            };
        if let Some((p, _)) = &self.running {
            let phase = scan::PHASES[p.phase.load(Ordering::Relaxed) as usize];
            let (done, total) = (p.hashed.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed));
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("{phase}… {} files found", p.files_seen.load(Ordering::Relaxed)));
            });
            if total > 0 {
                ui.add(egui::ProgressBar::new(done as f32 / total as f32).text(format!("{done} / {total}")));
            }
            if ui.button("Cancel").clicked() {
                p.cancel.store(true, Ordering::Relaxed);
            }
        } else if ui.add_enabled(ready, egui::Button::new(RichText::new("🔍 Scan").heading())).clicked() {
            self.start_scan();
        }
        ui.separator();
        egui::CollapsingHeader::new("Schedule & reports").show(ui, |ui| self.sidebar_schedule(ui, ready));
        egui::CollapsingHeader::new("Safety").show(ui, |ui| self.sidebar_safety(ui));
    }

    fn sidebar_where(&mut self, ui: &mut egui::Ui) {
        ui.radio_value(&mut self.mode, Mode::Dirs, "Selected directories");
        ui.radio_value(&mut self.mode, Mode::TwoDirs, "Compare two directories");
        ui.radio_value(&mut self.mode, Mode::System, "Whole system");
        if self.mode != Mode::System {
            let mut rm = None;
            for (i, r) in self.roots.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.small_button("✖").clicked() {
                        rm = Some(i);
                    }
                    let tag = if self.mode == Mode::TwoDirs { ["A: ", "B: "].get(i).copied().unwrap_or("") } else { "" };
                    ui.label(format!("{tag}{}", r.display()));
                });
            }
            if let Some(i) = rm {
                self.roots.remove(i);
            }
            if ui.button("➕ Add folder…").clicked()
                && let Some(p) = rfd::FileDialog::new().pick_folder()
            {
                self.roots.push(p);
            }
            if self.mode == Mode::TwoDirs && self.roots.len() != 2 {
                ui.colored_label(Self::warn(ui), "Pick exactly 2 folders (A, B)");
            }
        }
        ui.checkbox(&mut self.hidden, "Include hidden files");
        ui.label("Exclude (path substrings, ;-separated):");
        ui.text_edit_multiline(&mut self.exclude);
    }

    fn sidebar_what(&mut self, ui: &mut egui::Ui) {
        ui.label("Duplicates match on:");
        ui.checkbox(&mut self.crit.name, "Name");
        ui.checkbox(&mut self.crit.size, "Size");
        ui.checkbox(&mut self.crit.mtime, "Modified date");
        ui.checkbox(&mut self.crit.content, "Content (BLAKE3 hash)");
        ui.label("Also find:");
        ui.checkbox(&mut self.find_dirs, "Duplicate folders");
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.find_images, "Similar images, distance");
            ui.add(egui::DragValue::new(&mut self.img_dist).range(0..=20));
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.find_audio, "Similar music (tags), ±s");
            ui.add(egui::DragValue::new(&mut self.audio_tol).range(0..=30));
        });
        ui.horizontal(|ui| {
            ui.label("Min size KiB");
            ui.add(egui::DragValue::new(&mut self.min_kib));
            ui.label("Largest: top");
            ui.add(egui::DragValue::new(&mut self.top_n).range(1..=100_000));
        });
    }

    fn sidebar_schedule(&mut self, ui: &mut egui::Ui, ready: bool) {
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("freq").selected_text(self.sched_freq).width(70.0).show_ui(ui, |ui| {
                for f in ["hourly", "daily", "weekly"] {
                    ui.selectable_value(&mut self.sched_freq, f, f);
                }
            });
            if ui
                .add_enabled(ready, egui::Button::new("⏰ Schedule"))
                .on_hover_text("Run this scan periodically in the background; open results with Load report")
                .clicked()
            {
                let a = cli::to_args(&self.scan_opts(), self.crit, self.mode == Mode::TwoDirs);
                self.log.push(cli::schedule(self.sched_freq, &a).unwrap_or_else(|e| e));
            }
            if ui.small_button("Unschedule").clicked() {
                self.log.push(cli::unschedule().unwrap_or_else(|e| e));
            }
        });
        if ui.button("📂 Load report…").clicked()
            && let Some(p) = rfd::FileDialog::new().set_directory(cli::reports_dir()).add_filter("report", &["json"]).pick_file()
        {
            match std::fs::read_to_string(&p).map_err(|e| e.to_string()).and_then(|s| serde_json::from_str::<Report>(&s).map_err(|e| e.to_string())) {
                Ok(r) => {
                    self.res = Some(ScanResult { roots: r.roots, groups: r.groups, largest: r.largest, scanned: r.scanned, secs: r.secs, ..Default::default() });
                    self.tab = Tab::Dups;
                    self.sel.clear();
                }
                Err(e) => self.log.push(format!("report: {e}")),
            }
        }
    }

    fn sidebar_safety(&mut self, ui: &mut egui::Ui) {
        ui.label("Protected folders (never removed):");
        let mut rm = None;
        for (i, r) in self.journal.settings.protected.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.small_button("✖").clicked() {
                    rm = Some(i);
                }
                ui.label(format!("🔒 {}", r.display()));
            });
        }
        let mut changed = false;
        if let Some(i) = rm {
            self.journal.settings.protected.remove(i);
            changed = true;
        }
        if ui.button("➕ Protect folder…").clicked()
            && let Some(p) = rfd::FileDialog::new().pick_folder()
        {
            self.journal.settings.protected.push(p);
            changed = true;
        }
        changed |= ui.checkbox(&mut self.journal.settings.verify_bytes, "Byte-by-byte verify before removing").changed();
        if changed {
            self.save_settings();
        }
        ui.label(format!("Hash cache: {} files", self.cache.len()));
    }

    /// Bottom status bar: last log line, expandable to the full log.
    fn log_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let arrow = if self.log_open { "▾ Log" } else { "▸ Log" };
            if ui.selectable_label(self.log_open, format!("{arrow} ({})", self.log.len())).clicked() {
                self.log_open = !self.log_open;
            }
            if ui.small_button("clear").clicked() {
                self.log.clear();
            }
            if !self.log_open
                && let Some(l) = self.log.last()
            {
                ui.add(egui::Label::new(RichText::new(l).small()).truncate());
            }
        });
        if self.log_open {
            egui::ScrollArea::vertical().id_salt("log").max_height(160.0).stick_to_bottom(true).show(ui, |ui| {
                for l in &self.log {
                    ui.small(l);
                }
            });
        }
    }

    fn groups_of(res: &ScanResult, tab: Tab) -> &Vec<Group> {
        match tab {
            Tab::Folders => &res.dirs,
            Tab::Images => &res.images,
            Tab::Audio => &res.audio,
            _ => &res.groups,
        }
    }

    fn groups_view(&mut self, ui: &mut egui::Ui, tab: Tab) {
        let Some(res) = &self.res else {
            ui.label("Run a scan.");
            return;
        };
        let groups = Self::groups_of(res, tab);
        let wasted: u64 = groups.iter().map(|g| g[0].size * (g.len() as u64 - 1)).sum();
        let sel_n = groups.iter().flatten().filter(|f| self.sel.contains(&f.path)).count();
        let sel_bytes: u64 = groups.iter().flatten().filter(|f| self.sel.contains(&f.path)).map(|f| f.size).sum();
        ui.label(format!(
            "{} files scanned in {:.1}s · {} groups · {} reclaimable · {sel_n} selected ({})",
            res.scanned,
            res.secs,
            groups.len(),
            human(wasted),
            human(sel_bytes)
        ));
        if tab == Tab::Images || tab == Tab::Audio {
            ui.colored_label(Self::warn(ui), "Similar ≠ identical: removed files can't be rebuilt from the kept one. Use Trash or Quarantine.");
        }
        ui.horizontal_wrapped(|ui| {
            ui.strong("Select");
            ui.label("Filter:");
            ui.add(egui::TextEdit::singleline(&mut self.filter).desired_width(160.0).hint_text("path contains…"));
            ui.label("Sort:");
            egui::ComboBox::from_id_salt("sort").selected_text(format!("{:?}", self.sort)).show_ui(ui, |ui| {
                for s in [Sort::Wasted, Sort::Size, Sort::Count, Sort::Path] {
                    ui.selectable_value(&mut self.sort, s, format!("{s:?}"));
                }
            });
            ui.separator();
            ui.label("Auto-select, keep:");
            egui::ComboBox::from_id_salt("keep").selected_text(format!("{:?}", self.keep)).show_ui(ui, |ui| {
                for k in [KeepRule::Oldest, KeepRule::Newest, KeepRule::ShortestPath, KeepRule::FirstRoot] {
                    ui.selectable_value(&mut self.keep, k, format!("{k:?}"));
                }
            });
            if ui.button("Apply rule").clicked() {
                self.sel.extend(auto_select(groups, self.keep, &self.journal.settings.protected));
            }
            if ui.button("Clear").clicked() {
                self.sel.clear();
            }
            if ui.button("Collapse all").clicked() {
                self.collapsed.extend((0..groups.len()).map(|gi| (tab as u8, gi)));
            }
            if ui.button("Expand all").clicked() {
                self.collapsed.retain(|(t, _)| *t != tab as u8);
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.strong("Act    ");
            let actions: &[Action] = if tab == Tab::Folders {
                &[Action::Trash, Action::Delete, Action::MoveTo]
            } else {
                &[Action::Trash, Action::Delete, Action::Hardlink, Action::Reflink, Action::MoveTo]
            };
            if !actions.contains(&self.action) {
                self.action = Action::Trash;
            }
            egui::ComboBox::from_id_salt("action").selected_text(format!("{:?}", self.action)).show_ui(ui, |ui| {
                for &a in actions {
                    let r = ui.selectable_value(&mut self.action, a, format!("{a:?}"));
                    match a {
                        Action::Hardlink => r.on_hover_text("All copies become one file: they share the kept copy's permissions, owner and modified date, and editing one edits all."),
                        Action::Reflink => r.on_hover_text("Copy-on-write clone: keeps permissions and modified date, but the owner becomes you."),
                        _ => r,
                    };
                }
            });
            if ui.add_enabled(sel_n > 0, egui::Button::new("Run on selected…")).clicked() {
                self.confirm = Some(tab);
            }
            let st = &mut self.journal.settings;
            let mut changed = ui
                .checkbox(&mut st.quarantine, "Quarantine")
                .on_hover_text("Delete moves files to a quarantine folder instead; undo is instant and lossless. Space is freed only when purged.")
                .changed();
            if st.quarantine {
                ui.label("purge after");
                changed |= ui.add(egui::DragValue::new(&mut st.retention_days).range(1..=3650).suffix(" d")).changed();
            }
            if changed && let Err(e) = self.journal.save() {
                self.log.push(format!("settings: {e}"));
            }
            if ui.button("Export CSV…").clicked()
                && let Some(p) = rfd::FileDialog::new().set_file_name("duplicates.csv").save_file()
                && let Err(e) = std::fs::write(&p, to_csv(groups))
            {
                self.log.push(format!("csv: {e}"));
            }
        });
        ui.separator();
        let f = self.filter.to_lowercase();
        let mut order: Vec<usize> = (0..groups.len()).filter(|&gi| f.is_empty() || groups[gi].iter().any(|x| x.path.to_string_lossy().to_lowercase().contains(&f))).collect();
        let wasted = |g: &Group| g[0].size * (g.len() as u64 - 1);
        match self.sort {
            Sort::Wasted => order.sort_by_key(|&i| std::cmp::Reverse(wasted(&groups[i]))),
            Sort::Size => order.sort_by_key(|&i| std::cmp::Reverse(groups[i][0].size)),
            Sort::Count => order.sort_by_key(|&i| std::cmp::Reverse(groups[i].len())),
            Sort::Path => order.sort_by(|&a, &b| groups[a][0].path.cmp(&groups[b][0].path)),
        }
        let mut rows = Vec::with_capacity(order.len() * 3);
        for gi in order {
            rows.push(Row::Head(gi));
            if !self.collapsed.contains(&(tab as u8, gi)) {
                rows.extend((0..groups[gi].len()).map(|fi| Row::File(gi, fi)));
            }
        }
        let (mut open, mut protect, mut keep_only) = (None, None, None);
        let two = res.roots.len() == 2 && self.mode == Mode::TwoDirs;
        let prot = &self.journal.settings.protected;
        let row_h = if tab == Tab::Images { THUMB + 2.0 } else { ui.spacing().interact_size.y };
        egui::ScrollArea::vertical().id_salt(("groups", tab as u8)).auto_shrink(false).show_rows(ui, row_h, rows.len(), |ui, range| {
            for row in &rows[range] {
                ui.horizontal(|ui| {
                    ui.set_min_height(row_h);
                    match *row {
                        Row::Head(gi) => {
                            let g = &groups[gi];
                            let key = (tab as u8, gi);
                            let open_now = !self.collapsed.contains(&key);
                            let head = format!("{} {} × {} — {} wasted", if open_now { "▾" } else { "▸" }, g.len(), human(g[0].size), human(wasted(g)));
                            if ui.selectable_label(false, RichText::new(head).strong()).clicked() {
                                if open_now { self.collapsed.insert(key) } else { self.collapsed.remove(&key) };
                            }
                            if tab == Tab::Dups && g[0].size < 50 << 20 && is_text(&g[0].path) && ui.small_button("Compare / merge first two").clicked() {
                                open = Some((g[0].path.clone(), g[1].path.clone()));
                            }
                        }
                        Row::File(gi, fi) => {
                            let fe = &groups[gi][fi];
                            ui.add_space(16.0);
                            let locked = is_protected(&fe.path, prot);
                            let mut on = self.sel.contains(&fe.path);
                            if ui.add_enabled(!locked, egui::Checkbox::without_text(&mut on)).changed() {
                                if on { self.sel.insert(fe.path.clone()) } else { self.sel.remove(&fe.path) };
                            }
                            if ui.small_button("keep").on_hover_text("Keep this copy, select the others").clicked() {
                                keep_only = Some((gi, fi));
                            }
                            if tab == Tab::Images {
                                Self::thumb(&mut self.thumbs, &self.thumb_ch.0, ui, &fe.path);
                            }
                            let mut txt = format!("{}  {}  {}", chrono_lite::Dt::from(fe.mtime), human(fe.size), fe.path.display());
                            if two {
                                txt = format!("[{}] {txt}", ["A", "B"][fe.root.min(1)]);
                            }
                            if locked {
                                txt = format!("🔒 {txt}");
                            }
                            let r = ui.link(txt).on_hover_text("Open containing folder · right-click for more");
                            if r.clicked() {
                                let _ = open::that(fe.path.parent().unwrap_or(&fe.path));
                            }
                            if let Some(d) = Self::file_menu(&r, &fe.path) {
                                protect = Some(d);
                            }
                        }
                    }
                });
            }
        });
        if let Some((gi, fi)) = keep_only {
            for (i, fe) in groups[gi].iter().enumerate() {
                if i == fi || is_protected(&fe.path, prot) {
                    self.sel.remove(&fe.path);
                } else {
                    self.sel.insert(fe.path.clone());
                }
            }
        }
        if let Some(d) = protect {
            self.protect(d);
        }
        if let Some((l, r)) = open {
            self.open_merge(l, r, None);
        }
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(tab) = self.confirm else { return };
        let Some(res) = &self.res else { return };
        let groups = Self::groups_of(res, tab);
        let eff = self.effective_action(tab);
        let n = groups.iter().flatten().filter(|f| self.sel.contains(&f.path)).count();
        let (mut go, mut cancel) = (false, false);
        egui::Window::new("Confirm").collapsible(false).resizable(false).show(ctx, |ui| {
            ui.label(format!("{eff:?} {n} items?"));
            if removes_all_copies(groups, &self.sel) {
                ui.colored_label(Self::err(ui), "⚠ Some groups have ALL copies selected — those groups will be skipped.");
            }
            if matches!(eff, Action::Hardlink | Action::Reflink) {
                ui.colored_label(
                    Self::warn(ui),
                    if eff == Action::Hardlink {
                        "Hardlink: replaced files take the kept copy's permissions, owner and modified date."
                    } else {
                        "Reflink: replaced files keep permissions and modified date, but the owner becomes you."
                    },
                );
            }
            if tab == Tab::Folders {
                ui.colored_label(
                    Self::warn(ui),
                    "Folders were compared only on files the scan saw. Hidden files, files under the min size and excluded paths (.git, node_modules…) were NOT checked and are removed with the folder. Quarantined folders are purged after the retention period.",
                );
            }
            if tab == Tab::Dups && !self.crit.content {
                ui.colored_label(Self::warn(ui), "Content not compared: files that differ from the kept copy can't be rebuilt by undo (links refuse them).");
            }
            if eff == Action::Delete {
                ui.colored_label(Self::err(ui), "Permanent delete: undo only works by re-copying the kept duplicate.");
            }
            ui.horizontal(|ui| {
                go = ui.button("Proceed").clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if cancel {
            self.confirm = None;
        }
        if !go {
            return;
        }
        self.confirm = None;
        let batch = new_batch();
        let dest = match eff {
            Action::MoveTo => rfd::FileDialog::new().set_title("Move duplicates to…").pick_folder(),
            Action::Quarantine => Some(self.journal.quarantine_dir(batch)),
            _ => None,
        };
        if dest.is_none() && matches!(eff, Action::MoveTo) {
            return;
        }
        let o = ApplyOpts { action: Some(eff), dest, batch, protected: self.journal.settings.protected.clone(), verify_bytes: self.journal.settings.verify_bytes };
        let (done, errs) = apply(groups, &self.sel, &o);
        self.log.push(format!("{eff:?}: {} done, {} errors (undo in History)", done.len(), errs.len()));
        self.log.extend(errs);
        let gone: Vec<PathBuf> = done.iter().map(|e| e.orig.clone()).collect();
        if let Err(e) = self.journal.record(done) {
            self.log.push(format!("JOURNAL WRITE FAILED, undo unavailable: {e}"));
        }
        // Drop handled items (and anything inside removed folders) from every view.
        self.sel.retain(|p| !gone.iter().any(|x| p.starts_with(x)));
        if let Some(res) = &mut self.res {
            for list in [&mut res.groups, &mut res.dirs, &mut res.images, &mut res.audio] {
                for g in list.iter_mut() {
                    g.retain(|f| !gone.iter().any(|x| f.path.starts_with(x)));
                }
                list.retain(|g| g.len() > 1);
            }
        }
    }

    fn history_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(format!("Quarantine: {} · data dir {}", human(self.journal.quarantine_bytes()), self.journal.dir.display()));
            if ui.button("Empty quarantine…").clicked() {
                self.confirm_purge = true;
            }
        });
        ui.separator();
        let mut batches: Vec<u64> = self.journal.entries.iter().map(|e| e.batch).collect();
        batches.sort_unstable_by(|a, b| b.cmp(a));
        batches.dedup();
        let mut undo = None;
        egui::ScrollArea::vertical().id_salt("hist").show(ui, |ui| {
            for b in batches {
                let es: Vec<_> = self.journal.entries.iter().filter(|e| e.batch == b).collect();
                let open = es.iter().filter(|e| !e.undone).count();
                let head = format!(
                    "{} · {:?} · {} items · {}{}",
                    chrono_lite::Dt::from(es[0].time),
                    es[0].action,
                    es.len(),
                    human(es.iter().map(|e| e.size).sum()),
                    if open == 0 { " · undone" } else { "" }
                );
                ui.horizontal(|ui| {
                    if ui.add_enabled(open > 0, egui::Button::new("↶ Undo")).clicked() {
                        undo = Some(b);
                    }
                    egui::CollapsingHeader::new(head).id_salt(b).show(ui, |ui| {
                        for e in &es {
                            let state = if e.undone {
                                "restored"
                            } else if e.stash.is_some() {
                                "stashed"
                            } else if e.keep.is_some() {
                                "rebuildable"
                            } else if matches!(e.action, Action::Trash | Action::Created) {
                                "undoable"
                            } else {
                                "unrecoverable"
                            };
                            ui.label(format!("[{state}] {}", e.orig.display()));
                        }
                    });
                });
            }
        });
        if let Some(b) = undo {
            let errs = self.journal.undo_batch(b);
            self.log.push(format!("undo: {} errors (rescan to refresh views)", errs.len()));
            self.log.extend(errs);
        }
    }

    fn purge_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_purge {
            return;
        }
        egui::Window::new("Empty quarantine").collapsible(false).resizable(false).show(ctx, |ui| {
            ui.colored_label(Self::err(ui), format!("Permanently delete {} of quarantined files and backups?", human(self.journal.quarantine_bytes())));
            ui.label("Undo will afterwards only work by re-copying kept duplicates.");
            ui.horizontal(|ui| {
                if ui.button("Delete permanently").clicked() {
                    let r = self.journal.purge(0);
                    self.log.push(format!("purge: {r:?}"));
                    self.confirm_purge = false;
                }
                if ui.button("Cancel").clicked() {
                    self.confirm_purge = false;
                }
            });
        });
    }

    fn largest_view(&mut self, ui: &mut egui::Ui) {
        let Some(res) = &self.res else {
            ui.label("Run a scan.");
            return;
        };
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.treemap, true, "Treemap");
            ui.selectable_value(&mut self.treemap, false, "Largest files");
        });
        if self.treemap {
            match &res.tree {
                Some(t) => {
                    if let Some(n) = treemap_view(ui, t, self.tm_node) {
                        self.tm_node = n;
                    }
                }
                None => {
                    ui.label("No tree for a loaded report. Rescan for the treemap.");
                }
            }
            return;
        }
        let (mut trashed, mut protect) = (vec![], None);
        let row_h = ui.spacing().interact_size.y;
        egui::ScrollArea::vertical().id_salt("largest").auto_shrink(false).show_rows(ui, row_h, res.largest.len(), |ui, range| {
            for f in &res.largest[range] {
                ui.horizontal(|ui| {
                    if ui.small_button("🗑 Trash").clicked() {
                        trashed.push(f.clone());
                    }
                    ui.add_sized([80.0, row_h], egui::Label::new(human(f.size)));
                    ui.label(chrono_lite::Dt::from(f.mtime).to_string());
                    let r = ui.link(f.path.display().to_string()).on_hover_text("Open containing folder · right-click for more");
                    if r.clicked() {
                        let _ = open::that(f.path.parent().unwrap_or(&f.path));
                    }
                    if let Some(d) = Self::file_menu(&r, &f.path) {
                        protect = Some(d);
                    }
                });
            }
        });
        if let Some(d) = protect {
            self.protect(d);
        }
        for f in trashed {
            if is_protected(&f.path, &self.journal.settings.protected) {
                self.log.push(format!("{}: protected", f.path.display()));
                continue;
            }
            match undo::trash_file(&f.path) {
                Ok(stash) => {
                    self.log.push(format!("trashed {}", f.path.display()));
                    let _ = self.journal.record(vec![undo::Entry {
                        batch: new_batch(),
                        time: std::time::SystemTime::now(),
                        action: Action::Trash,
                        orig: f.path.clone(),
                        keep: None,
                        hash: None,
                        size: f.size,
                        mtime: f.mtime,
                        perms: 0,
                        stash,
                        undone: false,
                    }]);
                    if let Some(res) = &mut self.res {
                        res.largest.retain(|x| x.path != f.path);
                    }
                }
                Err(e) => self.log.push(format!("{}: {e}", f.path.display())),
            }
        }
    }

    fn sync_view(&mut self, ui: &mut egui::Ui) {
        let Some(res) = &self.res else {
            ui.label("Run a scan in “Compare two directories” mode.");
            return;
        };
        if res.sync.is_empty() {
            ui.label("Sync view needs a “Compare two directories” scan.");
            return;
        }
        let count = |s: sync::State| res.sync.iter().filter(|r| r.state == s).count();
        ui.label(format!("A = {}   B = {}", res.roots[0].display(), res.roots[1].display()));
        ui.horizontal_wrapped(|ui| {
            for (i, s) in SYNC_STATES.iter().enumerate() {
                ui.checkbox(&mut self.sync_show[i], format!("{s:?} ({})", count(*s)));
            }
        });
        let mut copy: Option<bool> = None; // Some(true) = A→B
        ui.horizontal(|ui| {
            if ui.button("Select all missing/newer").clicked() {
                self.sync_sel = res.sync.iter().filter(|r| !matches!(r.state, sync::State::Same | sync::State::Differ)).map(|r| r.rel.clone()).collect();
            }
            if ui.button("Clear").clicked() {
                self.sync_sel.clear();
            }
            ui.separator();
            if ui.button("Copy selected A → B").on_hover_text("Rows that exist in A. Overwritten files are backed up; undo in History.").clicked() {
                copy = Some(true);
            }
            if ui.button("Copy selected B → A").clicked() {
                copy = Some(false);
            }
        });
        ui.separator();
        let show = self.sync_show;
        let visible: Vec<&sync::Row> = res.sync.iter().filter(|r| show[SYNC_STATES.iter().position(|s| *s == r.state).unwrap()]).collect();
        let (mut open, mut protect) = (None, None);
        let (blue, green) = if ui.visuals().dark_mode {
            (Color32::LIGHT_BLUE, Color32::LIGHT_GREEN)
        } else {
            (Color32::from_rgb(0, 80, 190), Color32::from_rgb(0, 120, 40))
        };
        let (gray, warn) = (ui.visuals().weak_text_color(), Self::warn(ui));
        let row_h = ui.spacing().interact_size.y;
        egui::ScrollArea::vertical().id_salt("sync").show_rows(ui, row_h, visible.len(), |ui, range| {
            for r in &visible[range] {
                ui.horizontal(|ui| {
                    let mut on = self.sync_sel.contains(&r.rel);
                    if ui.checkbox(&mut on, "").changed() {
                        if on { self.sync_sel.insert(r.rel.clone()) } else { self.sync_sel.remove(&r.rel) };
                    }
                    let (c, sym) = match r.state {
                        sync::State::OnlyA => (blue, "A only "),
                        sync::State::OnlyB => (green, "B only "),
                        sync::State::Same => (gray, "same   "),
                        sync::State::NewerA => (blue, "A newer"),
                        sync::State::NewerB => (green, "B newer"),
                        sync::State::Differ => (warn, "differ "),
                    };
                    ui.colored_label(c, RichText::new(sym).monospace());
                    let lbl = ui.add(egui::Label::new(r.rel.display().to_string()).sense(egui::Sense::click()));
                    if let Some(fe) = r.a.as_ref().or(r.b.as_ref())
                        && let Some(d) = Self::file_menu(&lbl, &fe.path)
                    {
                        protect = Some(d);
                    }
                    if let (Some(a), Some(b)) = (&r.a, &r.b)
                        && r.state != sync::State::Same
                        && a.size < 50 << 20
                        && is_text(&a.path)
                        && ui.small_button("diff/merge").clicked()
                    {
                        open = Some((a.path.clone(), b.path.clone()));
                    }
                });
            }
        });
        if let Some(d) = protect {
            self.protect(d);
        }
        if let Some((a, b)) = open {
            self.open_merge(a, b, None);
        }
        if let Some(a_to_b) = copy {
            let batch = new_batch();
            let (mut done, mut errs) = (vec![], vec![]);
            let res = self.res.as_mut().unwrap();
            for r in res.sync.iter_mut().filter(|r| self.sync_sel.contains(&r.rel)) {
                let (src, dst_root) = if a_to_b { (&r.a, &res.roots[1]) } else { (&r.b, &res.roots[0]) };
                let Some(src) = src.clone() else { continue };
                let dst = dst_root.join(&r.rel);
                if is_protected(&dst, &self.journal.settings.protected) {
                    errs.push(format!("{}: protected", dst.display()));
                    continue;
                }
                match self.journal.copy_over(&src.path, &dst, batch) {
                    Ok(e) => {
                        done.push(e);
                        let copied = FileEntry { path: dst, root: a_to_b as usize, ..src };
                        if a_to_b { r.b = Some(copied) } else { r.a = Some(copied) }
                        r.state = sync::State::Same;
                    }
                    Err(e) => errs.push(format!("{}: {e}", dst.display())),
                }
            }
            self.log.push(format!("sync copy: {} done, {} errors (undo in History)", done.len(), errs.len()));
            self.log.extend(errs);
            if let Err(e) = self.journal.record(done) {
                self.log.push(format!("JOURNAL WRITE FAILED: {e}"));
            }
            self.sync_sel.clear();
        }
    }

    fn merge_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Pick two text files…").clicked()
                && let Some(l) = rfd::FileDialog::new().set_title("Left file").pick_file()
                && let Some(r) = rfd::FileDialog::new().set_title("Right file").pick_file()
            {
                self.open_merge(l, r, None);
            }
            if let Some(ms) = &self.merge
                && ui.button("Set common base (3-way)…").on_hover_text("The version both files came from; one-sided changes then merge automatically").clicked()
                && let Some(b) = rfd::FileDialog::new().set_title("Base (ancestor) file").pick_file()
            {
                let (l, r) = (ms.left.clone(), ms.right.clone());
                self.open_merge(l, r, Some(b));
            }
        });
        let Some(ms) = &mut self.merge else { return };
        ui.label(format!(
            "L: {}\nR: {}{}\n{} differing hunks, {} conflicts",
            ms.left.display(),
            ms.right.display(),
            ms.base.as_ref().map(|b| format!("\nBase: {}", b.display())).unwrap_or_default(),
            ms.m.diffs(),
            ms.m.conflicts()
        ));
        let mut save = None;
        ui.horizontal_wrapped(|ui| {
            ui.label("All conflicts:");
            for (p, t) in [(Pick::Left, "Accept left"), (Pick::Right, "Accept right"), (Pick::LeftThenRight, "Both L+R")] {
                if ui.button(t).clicked() {
                    ms.m.pick_all(p);
                    ms.edited = None;
                }
            }
            ui.separator();
            if ui.button("💾 Save to left").clicked() {
                save = Some(ms.left.clone());
            }
            if ui.button("💾 Save to right").clicked() {
                save = Some(ms.right.clone());
            }
            if ui.button("💾 Save as…").clicked() {
                save = rfd::FileDialog::new().save_file();
            }
            let mut manual = ms.edited.is_some();
            if ui.checkbox(&mut manual, "Hand-edit result").changed() {
                ms.edited = manual.then(|| ms.m.result());
            }
        });
        if let Some(p) = save {
            let txt = ms.edited.clone().unwrap_or_else(|| ms.m.result());
            let r = self.journal.save_text(&p, &txt);
            self.log.push(match r {
                Ok(()) => format!("saved {} (undo in History)", p.display()),
                Err(e) => format!("{}: {e}", p.display()),
            });
            return;
        }
        ui.separator();
        let h = ui.available_height();
        let three = ms.base.is_some();
        let (red, green) = if ui.visuals().dark_mode {
            (Color32::from_rgb(220, 90, 90), Color32::from_rgb(90, 190, 90))
        } else {
            (Color32::from_rgb(170, 20, 20), Color32::from_rgb(0, 120, 30))
        };
        // Prefix each line with its 1-based number, starting at `from`.
        let numbered = |lines: &[String], from: usize| lines.iter().enumerate().map(|(i, l)| format!("{:>5} {}", from + i, l.trim_end_matches(['\r', '\n']))).collect::<Vec<_>>().join("\n");
        ui.columns(2, |c| {
            egui::ScrollArea::vertical().id_salt("hunks").max_height(h).show(&mut c[0], |ui| {
                let (mut ln_l, mut ln_r) = (1, 1);
                for (i, ch) in ms.m.chunks.iter_mut().enumerate() {
                    match ch {
                        Chunk::Same(l) => {
                            // Collapse long unchanged runs. Numbers are left-side line numbers.
                            let n = l.len();
                            let s = if n > 6 {
                                format!("{}\n      … {} same lines …\n{}", numbered(&l[..2], ln_l), n - 4, numbered(&l[n - 2..], ln_l + n - 2))
                            } else {
                                numbered(l, ln_l)
                            };
                            ui.label(RichText::new(s).monospace().weak());
                            ln_l += n;
                            ln_r += n;
                        }
                        Chunk::Diff { left, right, base, pick, conflict } => {
                            ui.group(|ui| {
                                if !*conflict {
                                    ui.small("auto-resolved");
                                }
                                if let Some(b) = base {
                                    ui.label(RichText::new(format!("base:\n{}", b.concat().trim_end())).monospace().weak());
                                }
                                ui.columns(2, |lr| {
                                    lr[0].small("left");
                                    lr[0].label(RichText::new(numbered(left, ln_l)).monospace().color(red));
                                    lr[1].small("right");
                                    lr[1].label(RichText::new(numbered(right, ln_r)).monospace().color(green));
                                });
                                ln_l += left.len();
                                ln_r += right.len();
                                ui.horizontal(|ui| {
                                    ui.push_id(i, |ui| {
                                        let mut opts = vec![(Pick::Left, "L"), (Pick::Right, "R"), (Pick::LeftThenRight, "L+R"), (Pick::RightThenLeft, "R+L")];
                                        if three {
                                            opts.push((Pick::Base, "base"));
                                        }
                                        opts.push((Pick::Neither, "none"));
                                        for (p, t) in opts {
                                            ui.radio_value(pick, p, t);
                                        }
                                    });
                                });
                            });
                        }
                    }
                }
            });
            c[1].label("Result:");
            egui::ScrollArea::vertical().id_salt("result").max_height(h).show(&mut c[1], |ui| match &mut ms.edited {
                Some(t) => {
                    ui.add(egui::TextEdit::multiline(t).code_editor().desired_width(f32::INFINITY));
                }
                None => {
                    ui.label(RichText::new(ms.m.result()).monospace());
                }
            });
        });
    }
}

/// Squarified, nested treemap (folders drawn inside folders, 3 levels deep).
/// Returns the node to navigate to, if clicked.
fn treemap_view(ui: &mut egui::Ui, t: &tree::Tree, cur: usize) -> Option<usize> {
    let cur = cur.min(t.nodes.len() - 1);
    let mut next = None;
    ui.horizontal(|ui| {
        if ui.add_enabled(t.nodes[cur].parent.is_some(), egui::Button::new("⬆ Up")).clicked() {
            next = t.nodes[cur].parent;
        }
        ui.label(format!("{} — {}", if cur == 0 { "all roots".into() } else { t.path(cur).display().to_string() }, human(t.nodes[cur].size)));
        ui.weak("click: zoom into folder · ctrl+click: open in file manager");
    });
    let (resp, painter) = ui.allocate_painter(ui.available_size(), egui::Sense::click());
    let mut hovered = None;
    draw_level(&painter, t, cur, resp.rect, 0, resp.hover_pos(), &mut hovered);
    if let Some(k) = hovered {
        let clicked = resp.clicked();
        resp.on_hover_text_at_pointer(format!("{}\n{}", t.path(k).display(), human(t.nodes[k].size)));
        if clicked {
            let dir = if t.nodes[k].dir { Some(k) } else { t.nodes[k].parent };
            if ui.input(|i| i.modifiers.ctrl) {
                let _ = open::that(t.path(dir.unwrap_or(k)));
            } else {
                next = dir;
            }
        }
    }
    next
}

const TM_DEPTH: usize = 3;

fn draw_level(painter: &egui::Painter, t: &tree::Tree, node: usize, r: egui::Rect, depth: usize, hover: Option<egui::Pos2>, hovered: &mut Option<usize>) {
    // ponytail: top 400 children per level; tiny slivers beyond that aren't visible anyway.
    let kids: Vec<usize> = t.nodes[node].children.iter().copied().filter(|&k| t.nodes[k].size > 0).take(400).collect();
    let vals: Vec<f64> = kids.iter().map(|&k| t.nodes[k].size as f64).collect();
    for (&k, q) in kids.iter().zip(tree::squarify(&vals, r.min.x as f64, r.min.y as f64, r.width() as f64, r.height() as f64)) {
        let rr = egui::Rect::from_min_size(egui::pos2(q[0] as f32, q[1] as f32), egui::vec2(q[2] as f32, q[3] as f32));
        if rr.width() < 1.0 || rr.height() < 1.0 {
            continue;
        }
        let n = &t.nodes[k];
        let h = n.name.as_encoded_bytes().iter().fold(7u32, |a, &b| a.wrapping_mul(31).wrapping_add(b as u32));
        let mut col: Color32 = egui::ecolor::Hsva::new((h % 360) as f32 / 360.0, if n.dir { 0.5 } else { 0.15 }, 0.85 - depth as f32 * 0.12, 1.0).into();
        let hit = hover.is_some_and(|p| rr.contains(p));
        if hit {
            *hovered = Some(k); // deeper levels overwrite → innermost wins
            col = col.gamma_multiply(1.25);
        }
        painter.rect_filled(rr.shrink(0.5), 2.0, col);
        let label = |txt: String| {
            painter.with_clip_rect(rr).text(rr.min + egui::vec2(3.0, 1.0), egui::Align2::LEFT_TOP, txt, egui::FontId::proportional(11.0), Color32::BLACK)
        };
        let inner = egui::Rect::from_min_max(rr.min + egui::vec2(3.0, 15.0), rr.max - egui::vec2(3.0, 3.0));
        if n.dir && depth + 1 < TM_DEPTH && !n.children.is_empty() && inner.width() > 24.0 && inner.height() > 16.0 {
            label(format!("{}/  {}", n.name.to_string_lossy(), human(n.size)));
            draw_level(painter, t, k, inner, depth + 1, hover, hovered);
        } else if rr.width() > 50.0 && rr.height() > 14.0 {
            label(format!("{}{}\n{}", n.name.to_string_lossy(), if n.dir { "/" } else { "" }, human(n.size)));
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        if let Some((_, rx)) = &self.running {
            match rx.try_recv() {
                Ok(r) => {
                    self.res = Some(r);
                    self.running = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.running = None,
                Err(_) => ui.ctx().request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
        let ctx = ui.ctx().clone();
        while let Ok((p, img)) = self.thumb_ch.1.try_recv() {
            let tex = ctx.load_texture(p.display().to_string(), img, Default::default());
            self.thumbs.insert(p, Some(tex));
        }
        self.confirm_dialog(&ctx);
        self.purge_dialog(&ctx);
        egui::Panel::left("side").default_size(330.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.sidebar(ui));
        });
        egui::Panel::bottom("log").show(ui, |ui| self.log_bar(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            let n = |f: fn(&ScanResult) -> usize| self.res.as_ref().map(f).unwrap_or(0);
            let tabs = [
                (Tab::Dups, format!("Duplicates ({})", n(|r| r.groups.len()))),
                (Tab::Folders, format!("Folders ({})", n(|r| r.dirs.len()))),
                (Tab::Images, format!("Images ({})", n(|r| r.images.len()))),
                (Tab::Audio, format!("Music ({})", n(|r| r.audio.len()))),
                (Tab::Largest, "Disk usage".into()),
                (Tab::Sync, "A ⇄ B sync".into()),
                (Tab::Merge, "Text compare / merge".into()),
                (Tab::History, "History / undo".into()),
            ];
            ui.horizontal_wrapped(|ui| {
                for (t, label) in tabs {
                    ui.selectable_value(&mut self.tab, t, label);
                }
            });
            ui.separator();
            match self.tab {
                Tab::Dups | Tab::Folders | Tab::Images | Tab::Audio => self.groups_view(ui, self.tab),
                Tab::Largest => self.largest_view(ui),
                Tab::Sync => self.sync_view(ui),
                Tab::Merge => self.merge_view(ui),
                Tab::History => self.history_view(ui),
            }
        });
    }
}

/// Minimal UTC date formatting; avoids pulling chrono for one label.
mod chrono_lite {
    use std::time::{SystemTime, UNIX_EPOCH};
    pub struct Dt(i64);
    impl From<SystemTime> for Dt {
        fn from(t: SystemTime) -> Self {
            Dt(t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0))
        }
    }
    impl std::fmt::Display for Dt {
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            // Howard Hinnant's civil_from_days.
            let z = self.0.div_euclid(86400) + 719468;
            let (era, doe) = (z.div_euclid(146097), z.rem_euclid(146097));
            let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let m = if mp < 10 { mp + 3 } else { mp - 9 };
            let y = yoe + era * 400 + (m <= 2) as i64;
            let s = self.0.rem_euclid(86400);
            write!(f, "{y:04}-{m:02}-{d:02} {:02}:{:02}", s / 3600, s / 60 % 60)
        }
    }
    #[test]
    fn date() {
        assert_eq!(Dt(1_700_000_000).to_string(), "2023-11-14 22:13");
    }
}
