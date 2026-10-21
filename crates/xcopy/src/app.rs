// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Retro Computing Club (RCC)

//! The X-COPY screen: layout, interaction and the operations started by its buttons.

use crate::amiga;
use crate::engine::{Engine, Level, Task};
use crate::font::{wrap, Font, Style};
use crate::gfx::{self, lerp, pal, Canvas, Gfx};
use crate::settings::*;
use eframe::egui::{self, Color32, CursorIcon, Id, Key, Rect, Sense, TextureHandle, Ui};
use hgf_core::amigados::{self, Filesystem, FormatOpts};
use hgf_core::gw::{Action, CleanOpts, EraseOpts, ReadOpts, ReadProfile, WriteOpts};
use hgf_core::{formats, locate, TrackSet, TrackState};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::Instant;

const STORAGE_KEY: &str = "xcopy-settings";

/// Everything a click on the screen can do.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Act {
    Start,
    Repeat,
    Stop,
    DiskInfo,
    Directory,
    CheckDisk,
    Default,
    Section(Section),
    Mode(i32),
    StartAdj(i32),
    EndAdj(i32),
    Side,
    Dens,
    Flow,
    ToolDevice,
    Fs,
    EditName,
    Source(usize),
    Target(usize),
    Setup,
    Log,
    ModalButton(usize),
    ModalField(usize),
    ModalScroll(i32),
    Bus,
    SoundToggle,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModalKind {
    /// Requester asked by the engine (confirm, insert disk, error, directory…).
    Engine,
    Setup,
    Name,
    Log,
}

struct Field {
    label: &'static str,
    value: String,
    max: usize,
}

struct Modal {
    kind: ModalKind,
    title: String,
    lines: Vec<(String, Level)>,
    buttons: Vec<&'static str>,
    fields: Vec<Field>,
    focus: usize,
    scroll: usize,
    error: Option<String>,
    bus: Bus,
    sound: bool,
}

impl Modal {
    fn new(kind: ModalKind, title: &str, lines: Vec<(String, Level)>, buttons: Vec<&'static str>) -> Self {
        Self {
            kind,
            title: title.into(),
            lines,
            buttons,
            fields: Vec::new(),
            focus: 0,
            scroll: 0,
            error: None,
            bus: Bus::Ibm,
            sound: true,
        }
    }
}

/// What to do with the file chosen in the (asynchronous) file dialog.
enum Pick {
    /// DF>ADF: read the source disk into this file.
    ReadTo,
    /// ADF>DF: write this image to the target disks.
    WriteFrom,
    /// TOOLS/FORMAT with DEVICE=ADF: save this blank image.
    SaveBlank(Vec<u8>),
    /// DIRECTORY of an image file.
    Dir,
}

/// Image kept after a copy, so REPEAT writes more copies without reading the source again.
#[derive(Clone)]
struct LastImage {
    path: PathBuf,
    format: String,
    tracks: hgf_core::gw::TrackOpts,
    geo: TrackSet,
}

pub struct App {
    s: Settings,
    eng: Engine,
    font: Font,
    dots: TextureHandle,
    modal: Option<Modal>,
    last_image: Option<LastImage>,
    pending_image: Option<(u64, LastImage)>,
    /// File dialog open in the background (never blocks the window).
    picking: Option<(Pick, Receiver<Option<PathBuf>>)>,
    temps: Vec<PathBuf>,
    detect_rx: Option<Receiver<Option<String>>>,
    gw_found: Option<bool>,
    help: Option<String>,
    status_seen: String,
    status_since: Instant,
    t0: Instant,
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("xcopy-modern-{}-{name}", std::process::id()))
}

/// Remove temporary images left by sessions that were killed (older than 12 hours).
fn clean_stale_temps() {
    let Ok(dir) = std::fs::read_dir(std::env::temp_dir()) else { return };
    for e in dir.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("xcopy-modern-") {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|d| d.as_secs() > 12 * 3600);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn level_color(l: Level) -> Color32 {
    match l {
        Level::Cmd => pal::CYAN,
        Level::Info => pal::CYAN,
        Level::Good => pal::GREEN,
        Level::Warn => pal::YELLOW,
        Level::Error => pal::RED,
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut s: Settings =
            cc.storage.and_then(|st| eframe::get_value(st, STORAGE_KEY)).unwrap_or_default();
        if let Some(p) = std::env::var_os("XCOPY_GW").or_else(|| std::env::var_os("HGF_GW")) {
            s.cfg.program = p.to_string_lossy().into_owned();
        }
        if s.end < s.start || s.end > MAX_CYL {
            s.start = 0;
            s.end = 79;
        }
        let ctx = &cc.egui_ctx;
        let mut app = Self {
            eng: Engine::new(s.cfg.clone()),
            s,
            font: Font::new(ctx),
            dots: gfx::dots_texture(ctx),
            modal: None,
            last_image: None,
            pending_image: None,
            picking: None,
            temps: Vec::new(),
            detect_rx: None,
            gw_found: None,
            help: None,
            status_seen: String::new(),
            status_since: Instant::now(),
            t0: Instant::now(),
        };
        clean_stale_temps();
        app.detect();
        app.eng.push_log("X-COPY Modern ready. Greaseweazle host tools: searching...", Level::Info);
        app
    }

    fn detect(&mut self) {
        let (tx, rx) = mpsc::channel();
        let pref = self.s.cfg.program.clone();
        std::thread::spawn(move || {
            let _ = tx.send(locate::detect(&pref));
        });
        self.detect_rx = Some(rx);
        self.gw_found = None;
    }

    fn poll_detect(&mut self) {
        let Some(rx) = &self.detect_rx else { return };
        let Ok(found) = rx.try_recv() else { return };
        self.detect_rx = None;
        match found {
            Some(p) => {
                self.eng.push_log(format!("Greaseweazle host tools: {p}"), Level::Good);
                self.s.cfg.program = p;
                self.gw_found = Some(true);
                if let Some(m) = &mut self.modal {
                    if m.kind == ModalKind::Setup {
                        m.fields[0].value = self.s.cfg.program.clone();
                        m.error = Some("GW FOUND.".into());
                    }
                }
            }
            None => {
                self.gw_found = Some(false);
                self.eng.push_log(
                    "gw not found: install the Greaseweazle host tools (pip install greaseweazle) or set the path in SETUP.",
                    Level::Error,
                );
                if !self.eng.busy() {
                    self.eng.set_status("GW NOT FOUND - CLICK SETUP (TOP RIGHT).", Level::Error);
                }
                if let Some(m) = &mut self.modal {
                    if m.kind == ModalKind::Setup {
                        m.error = Some("GW NOT FOUND ON THIS COMPUTER.".into());
                    }
                }
            }
        }
    }

    // ───────────────────────────── operations ─────────────────────────────

    fn notice(&mut self, title: &str, text: &str) {
        let lines = vec![(text.to_string(), Level::Warn)];
        self.modal = Some(Modal::new(ModalKind::Engine, title, lines, vec!["OK"]));
    }

    fn dname(i: usize) -> String {
        format!("DF{i}")
    }

    fn gw(&self, action: Action, idx: usize, label: String, geometry: TrackSet) -> Option<Task> {
        let drive = self.s.bus.drive(idx)?;
        Some(Task::Gw { action, drive, idx, label, geometry })
    }

    fn launch(&mut self, tasks: Vec<Task>) {
        self.eng.start(tasks, self.s.cfg.clone());
    }

    fn start_dir(&self) -> Option<PathBuf> {
        self.s.last_dir.clone().filter(|d| d.is_dir())
    }

    /// Open a file dialog without blocking the window: the window keeps repainting and can be
    /// resized or moved while the dialog is open (a blocking dialog inside the frame made
    /// winit crash on resize). The result is collected by [`App::poll_pick`].
    fn spawn_dialog(&mut self, purpose: Pick, save: Option<(String, &'static str, &'static str)>) {
        if self.picking.is_some() {
            return;
        }
        let mut d = rfd::AsyncFileDialog::new();
        if let Some(dir) = self.start_dir() {
            d = d.set_directory(dir);
        }
        let (tx, rx) = mpsc::channel();
        match save {
            Some((name, ext, what)) => {
                let fut = d.set_file_name(name).add_filter(what, &[ext]).save_file();
                std::thread::spawn(move || {
                    let _ = tx.send(pollster::block_on(fut).map(|h| h.path().to_path_buf()));
                });
            }
            None => {
                let fut = d
                    .add_filter("Amiga disk image", &["adf", "adz", "scp", "ipf", "hfe", "raw", "dmk"])
                    .add_filter("All files", &["*"])
                    .pick_file();
                std::thread::spawn(move || {
                    let _ = tx.send(pollster::block_on(fut).map(|h| h.path().to_path_buf()));
                });
            }
        }
        self.picking = Some((purpose, rx));
        self.eng.set_status("SELECT A FILE...", Level::Warn);
    }

    fn poll_pick(&mut self) {
        let Some((_, rx)) = &self.picking else { return };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => None,
        };
        let Some((purpose, _)) = self.picking.take() else { return };
        let Some(path) = result else {
            self.eng.set_status("NO FILE SELECTED.", Level::Warn);
            return;
        };
        self.s.last_dir = path.parent().map(Path::to_path_buf);
        match purpose {
            Pick::ReadTo => self.read_to_file(path),
            Pick::WriteFrom => self.write_from_file(path),
            Pick::SaveBlank(image) => match std::fs::write(&path, &image) {
                Ok(()) => self.eng.set_status(
                    format!(
                        "BLANK ADF SAVED: {}",
                        path.file_name().unwrap_or_default().to_string_lossy().to_uppercase()
                    ),
                    Level::Good,
                ),
                Err(e) => self.notice("SAVE FAILED", &e.to_string()),
            },
            Pick::Dir => {
                self.launch(vec![Task::Dir { path }, Task::Done { message: "OPERATION DONE.".into() }])
            }
        }
    }

    /// Tasks reading the source disk into `out` with the current copy mode. BAMCOPY first reads
    /// cylinder 40 (root block and bitmap), then only the cylinders holding allocated blocks.
    fn read_tasks(&self, out: &Path) -> Vec<Task> {
        let src = self.s.source;
        let Some(drive) = self.s.bus.drive(src) else { return Vec::new() };
        let nibble = self.s.copy_mode == CopyMode::Nibble;
        let format = if nibble { String::new() } else { self.s.sector_format().to_string() };
        let (tracks, geo) = self.s.tracks();
        let profile = if self.s.copy_mode == CopyMode::DosCopyPlus {
            ReadProfile::Recovery
        } else {
            ReadProfile::Standard
        };
        let label = format!("READING {}", Self::dname(src));
        let hd = self.s.hd;
        let mut v = Vec::new();
        if self.s.copy_mode == CopyMode::BamCopy {
            let probe = temp_path("bam.adf");
            let _ = std::fs::remove_file(&probe);
            let probe_read = Action::Read(ReadOpts {
                output: probe.clone(),
                format: format.clone(),
                tracks: hgf_core::gw::TrackOpts { custom: "c=40:h=0-1".into(), ..Default::default() },
                ..Default::default()
            });
            v.push(Task::Gw {
                action: probe_read,
                drive,
                idx: src,
                label: format!("READING BITMAP {}", Self::dname(src)),
                geometry: TrackSet { cyls: vec![40], heads: vec![0, 1] },
            });
            let out = out.to_path_buf();
            let (start, end) = (self.s.start, self.s.end);
            v.push(Task::Expand(Box::new(move |e: &mut Engine| {
                let data = std::fs::read(&probe).unwrap_or_default();
                let _ = std::fs::remove_file(&probe);
                let (tracks, geometry) = match amiga::used_cylinders(&data, hd) {
                    Ok(cyls) => {
                        let cyls: Vec<u16> = cyls.into_iter().filter(|c| (start..=end).contains(c)).collect();
                        e.push_log(
                            format!(
                                "BAMCOPY: {} cylinder(s) in use out of 80: {}",
                                cyls.len(),
                                amiga::set_notation(&cyls)
                            ),
                            Level::Good,
                        );
                        let t = hgf_core::gw::TrackOpts {
                            custom: format!("c={}:h=0-1", amiga::set_notation(&cyls)),
                            ..Default::default()
                        };
                        (t, TrackSet { cyls, heads: vec![0, 1] })
                    }
                    Err(err) => {
                        e.push_log(format!("BAMCOPY: {err} Reading the whole disk instead."), Level::Warn);
                        (tracks, geo)
                    }
                };
                let read = Action::Read(ReadOpts {
                    output: out.clone(),
                    format,
                    profile,
                    tracks,
                    ..Default::default()
                });
                let pad = out.clone();
                Ok(vec![
                    Task::Gw { action: read, drive, idx: src, label, geometry },
                    Task::Expand(Box::new(move |_| amiga::pad_image(&pad, hd).map(|_| Vec::new()))),
                ])
            })));
        } else {
            let read = Action::Read(ReadOpts {
                output: out.to_path_buf(),
                format,
                profile,
                tracks,
                ..Default::default()
            });
            v.push(Task::Gw { action: read, drive, idx: src, label, geometry: geo });
        }
        v
    }

    /// DF>ADF, once the destination file is chosen.
    fn read_to_file(&mut self, out: PathBuf) {
        if self.eng.busy() || !self.source_ok() {
            return;
        }
        let _ = std::fs::remove_file(&out);
        let file = out.file_name().map(|n| n.to_string_lossy().to_uppercase()).unwrap_or_default();
        let mut v = self.read_tasks(&out);
        if self.s.copy_mode != CopyMode::Nibble {
            v.push(Task::BootCheck { path: out.clone(), hd: self.s.hd });
        }
        v.push(Task::Done { message: format!("IMAGE SAVED: {file}") });
        self.launch(v);
    }

    /// ADF>DF, once the image is chosen.
    fn write_from_file(&mut self, input: PathBuf) {
        if self.eng.busy() || self.picking.is_some() {
            return;
        }
        let Some(targets) = self.targets_or_notice() else { return };
        let (tracks, geo) = self.s.tracks();
        let size = std::fs::metadata(&input).map(|m| m.len()).unwrap_or(0);
        let ext_in = formats::extension(&input).unwrap_or_default();
        let format = match formats::guess_format(&input, size) {
            Some(f) => f.to_string(),
            None if ext_in == "adf" => {
                return self.notice(
                    "BAD IMAGE",
                    &format!("THIS ADF IS {size} BYTES. EXPECTED 901120 (DD) OR 1802240 (HD)."),
                )
            }
            None => String::new(),
        };
        let geo = if format.is_empty() { TrackSet::dense(80, 2) } else { geo };
        let tracks = if format.is_empty() { Default::default() } else { tracks };
        let img = LastImage { path: input, format, tracks, geo };
        let mut v = vec![Self::destroy_confirm(&targets)];
        v.extend(self.write_tasks(&img, &targets, None, false));
        v.push(Task::Done { message: "OPERATION DONE. IMAGE WRITTEN.".into() });
        self.pending_image = Some((self.eng.done_count + 1, img));
        self.launch(v);
    }

    fn write_task(
        &self,
        input: &Path,
        format: &str,
        tracks: hgf_core::gw::TrackOpts,
        no_verify: bool,
        idx: usize,
        geo: TrackSet,
    ) -> Option<Task> {
        let action = Action::Write(WriteOpts {
            input: input.to_path_buf(),
            format: format.to_string(),
            tracks,
            no_verify,
            ..Default::default()
        });
        self.gw(action, idx, format!("WRITING {}", Self::dname(idx)), geo)
    }

    fn destroy_confirm(targets: &[usize]) -> Task {
        let list: Vec<String> = targets.iter().map(|&t| Self::dname(t)).collect();
        Task::Confirm {
            title: "WARNING!".into(),
            text: format!("ALL DATA ON {} WILL BE DESTROYED. CONTINUE?", list.join(", ")),
        }
    }

    fn source_ok(&mut self) -> bool {
        if self.s.bus.drive(self.s.source).is_none() {
            self.notice("NO SOURCE DRIVE", "SELECT A SOURCE DRIVE AVAILABLE ON THIS BUS (SEE SETUP).");
            return false;
        }
        true
    }

    fn targets_or_notice(&mut self) -> Option<Vec<usize>> {
        let t = self.s.target_list();
        if t.is_empty() {
            self.notice(
                "NO TARGET DRIVE",
                "CLICK A LAMP UNDER THE DRIVES TO SELECT AT LEAST ONE TARGET DRIVE.",
            );
            return None;
        }
        Some(t)
    }

    /// Writes of `image` to every target; the source drive (disk swap) goes last with a prompt.
    fn write_tasks(
        &self,
        img: &LastImage,
        targets: &[usize],
        swap_from: Option<usize>,
        no_verify: bool,
    ) -> Vec<Task> {
        let mut order: Vec<usize> = targets.iter().copied().filter(|&t| Some(t) != swap_from).collect();
        if let Some(s) = swap_from.filter(|s| targets.contains(s)) {
            order.push(s);
        }
        let mut v = Vec::new();
        for t in order {
            if Some(t) == swap_from {
                v.push(Task::Prompt {
                    title: "CHANGE DISK".into(),
                    text: format!("REMOVE THE SOURCE DISK AND INSERT THE TARGET DISK IN {}.", Self::dname(t)),
                });
            }
            v.extend(self.write_task(
                &img.path,
                &img.format,
                img.tracks.clone(),
                no_verify,
                t,
                img.geo.clone(),
            ));
        }
        v
    }

    fn start(&mut self) {
        if self.eng.busy() || self.picking.is_some() {
            return;
        }
        match self.s.section {
            Section::Copy => self.start_copy(),
            Section::Tools => self.start_tool(),
        }
    }

    fn start_copy(&mut self) {
        let nibble = self.s.copy_mode == CopyMode::Nibble;
        let format = if nibble { String::new() } else { self.s.sector_format().to_string() };
        let ext = if nibble { "scp" } else { "adf" };
        let (tracks, geo) = self.s.tracks();
        let src = self.s.source;
        match self.s.flow {
            Flow::DiskToDisk => {
                if !self.source_ok() {
                    return;
                }
                let Some(targets) = self.targets_or_notice() else { return };
                let temp = temp_path(&format!("copy.{ext}"));
                let _ = std::fs::remove_file(&temp);
                self.temps.push(temp.clone());
                // BAMCOPY writes the whole disk (unread tracks blank), so the target is clean.
                let (tracks, geo) = if self.s.copy_mode == CopyMode::BamCopy {
                    (Default::default(), TrackSet::dense(80, 2))
                } else {
                    (tracks, geo)
                };
                let mut v = vec![Self::destroy_confirm(&targets)];
                v.extend(self.read_tasks(&temp));
                v.push(Task::CheckBad);
                if !nibble {
                    v.push(Task::BootCheck { path: temp.clone(), hd: self.s.hd });
                }
                let img = LastImage { path: temp, format, tracks, geo };
                v.extend(self.write_tasks(&img, &targets, Some(src), false));
                v.push(Task::Done {
                    message: format!(
                        "OPERATION DONE. {} COP{} WRITTEN.",
                        targets.len(),
                        if targets.len() > 1 { "IES" } else { "Y" }
                    ),
                });
                self.pending_image = Some((self.eng.done_count + 1, img));
                self.launch(v);
            }
            Flow::DiskToAdf => {
                if !self.source_ok() {
                    return;
                }
                // Read cylinder 40 first: the volume name becomes the default file name.
                let probe = temp_path("probe.adf");
                let _ = std::fs::remove_file(&probe);
                self.temps.push(probe.clone());
                let read = Action::Read(ReadOpts {
                    output: probe.clone(),
                    format: self.s.sector_format().into(),
                    profile: ReadProfile::QuickScan,
                    tracks: hgf_core::gw::TrackOpts { custom: "c=40:h=0-1".into(), ..Default::default() },
                    ..Default::default()
                });
                let geo40 = TrackSet { cyls: vec![40], heads: vec![0, 1] };
                let mut v: Vec<Task> = self
                    .gw(read, src, format!("READING NAME {}", Self::dname(src)), geo40)
                    .into_iter()
                    .collect();
                v.push(Task::NameProbe { path: probe, hd: self.s.hd });
                self.launch(v);
            }
            Flow::AdfToDisk => {
                if self.targets_or_notice().is_some() {
                    self.spawn_dialog(Pick::WriteFrom, None);
                }
            }
        }
    }

    fn start_tool(&mut self) {
        let (tracks, geo) = self.s.tracks();
        match self.s.tool_mode {
            ToolMode::Format | ToolMode::QFormat => {
                let opts = FormatOpts {
                    name: self.s.volume.clone(),
                    filesystem: if self.s.ffs { Filesystem::Ffs } else { Filesystem::Ofs },
                    international: false,
                    hd: self.s.hd,
                };
                let image = match amigados::blank_image(&opts) {
                    Ok(i) => i,
                    Err(e) => return self.notice("BAD VOLUME NAME", &e),
                };
                if self.s.tool_adf {
                    let name = format!("{}.adf", self.s.volume.trim().replace(' ', "_"));
                    self.spawn_dialog(Pick::SaveBlank(image), Some((name, "adf", "Amiga disk file")));
                    return;
                }
                let Some(targets) = self.targets_or_notice() else { return };
                let temp = temp_path("format.adf");
                if let Err(e) = std::fs::write(&temp, &image) {
                    return self.notice("TEMP FILE", &e.to_string());
                }
                self.temps.push(temp.clone());
                let img = LastImage {
                    path: temp,
                    format: opts.gw_format().into(),
                    tracks: Default::default(),
                    geo: TrackSet::dense(80, 2),
                };
                let quick = self.s.tool_mode == ToolMode::QFormat;
                let mut v = vec![Self::destroy_confirm(&targets)];
                for t in &targets {
                    let mut task = self.write_task(
                        &img.path,
                        &img.format,
                        Default::default(),
                        quick,
                        *t,
                        img.geo.clone(),
                    );
                    if let Some(Task::Gw { label, .. }) = &mut task {
                        *label = format!("FORMATTING {}", Self::dname(*t));
                    }
                    v.extend(task);
                }
                v.push(Task::Done {
                    message: format!(
                        "FORMAT DONE: \"{}\" {}.",
                        self.s.volume.to_uppercase(),
                        if self.s.ffs { "FFS" } else { "OFS" }
                    ),
                });
                self.launch(v);
            }
            ToolMode::Track0 => {
                let mut opts = FormatOpts {
                    name: self.s.volume.clone(),
                    filesystem: if self.s.ffs { Filesystem::Ffs } else { Filesystem::Ofs },
                    international: false,
                    hd: self.s.hd,
                };
                if amigados::validate_name(&opts.name).is_err() {
                    opts.name = "Empty".into();
                }
                let image = match amigados::blank_image(&opts) {
                    Ok(i) => i,
                    Err(e) => return self.notice("TRACK 0", &e),
                };
                let Some(targets) = self.targets_or_notice() else { return };
                let temp = temp_path("track0.adf");
                if let Err(e) = std::fs::write(&temp, &image) {
                    return self.notice("TEMP FILE", &e.to_string());
                }
                self.temps.push(temp.clone());
                let list: Vec<String> = targets.iter().map(|&t| Self::dname(t)).collect();
                let mut v = vec![Task::Confirm {
                    title: "WARNING!".into(),
                    text: format!(
                        "TRACK 0 (BOOT BLOCK) OF {} WILL BE ERASED AND REWRITTEN. THE DISK WILL NO LONGER BOOT. CONTINUE?",
                        list.join(", ")
                    ),
                }];
                for &t in &targets {
                    let action = Action::Write(WriteOpts {
                        input: temp.clone(),
                        format: opts.gw_format().into(),
                        tracks: hgf_core::gw::TrackOpts { custom: "c=0:h=0-1".into(), ..Default::default() },
                        pre_erase: true,
                        retries: Some(8),
                        ..Default::default()
                    });
                    let geo = TrackSet { cyls: vec![0], heads: vec![0, 1] };
                    v.extend(self.gw(action, t, format!("REWRITING TRACK 0 {}", Self::dname(t)), geo));
                }
                v.push(Task::Done { message: "TRACK 0 REWRITTEN AND VERIFIED.".into() });
                self.launch(v);
            }
            ToolMode::Erase => {
                let Some(targets) = self.targets_or_notice() else { return };
                let mut v = vec![Self::destroy_confirm(&targets)];
                for &t in &targets {
                    let a = Action::Erase(EraseOpts { tracks: tracks.clone(), ..Default::default() });
                    v.extend(self.gw(a, t, format!("ERASING {}", Self::dname(t)), geo.clone()));
                }
                v.push(Task::Done { message: "ERASE DONE.".into() });
                self.launch(v);
            }
            ToolMode::Speed => {
                if !self.source_ok() {
                    return;
                }
                let src = self.s.source;
                let mut v: Vec<Task> = self
                    .gw(Action::Rpm { nr: 10 }, src, format!("SPEED CHECK {}", Self::dname(src)), geo)
                    .into_iter()
                    .collect();
                v.push(Task::RpmDone);
                self.launch(v);
            }
            ToolMode::Clean => {
                let Some(targets) = self.targets_or_notice() else { return };
                let t = targets[0];
                let mut v = vec![Task::Prompt {
                    title: "HEAD CLEANING".into(),
                    text: format!("INSERT A CLEANING DISK (WET IT WITH ISOPROPYL) IN {}.", Self::dname(t)),
                }];
                v.extend(self.gw(
                    Action::Clean(CleanOpts::default()),
                    t,
                    format!("CLEANING {}", Self::dname(t)),
                    geo,
                ));
                v.push(Task::Done { message: "CLEANING DONE.".into() });
                self.launch(v);
            }
        }
    }

    fn repeat(&mut self) {
        if self.eng.busy() || self.picking.is_some() {
            return;
        }
        let rewrite = self.s.section == Section::Copy && self.s.flow != Flow::DiskToAdf;
        if let (true, Some(img)) = (rewrite, self.last_image.clone()) {
            if !img.path.exists() {
                self.last_image = None;
                return self.start();
            }
            let Some(targets) = self.targets_or_notice() else { return };
            let list: Vec<String> = targets.iter().map(|&t| Self::dname(t)).collect();
            let mut v = vec![Task::Prompt {
                title: "REPEAT".into(),
                text: format!(
                    "INSERT NEW TARGET DISK(S) IN {}. THE SAME IMAGE WILL BE WRITTEN AGAIN.",
                    list.join(", ")
                ),
            }];
            v.extend(self.write_tasks(&img, &targets, None, false));
            v.push(Task::Done { message: "OPERATION DONE. ONE MORE COPY.".into() });
            self.launch(v);
        } else {
            self.start();
        }
    }

    fn check_disk(&mut self) {
        if self.eng.busy() || self.picking.is_some() || !self.source_ok() {
            return;
        }
        let (tracks, geo) = self.s.tracks();
        let temp = temp_path("check.adf");
        self.temps.push(temp.clone());
        let read = Action::Read(ReadOpts {
            output: temp,
            format: self.s.sector_format().into(),
            profile: ReadProfile::QuickScan,
            tracks,
            ..Default::default()
        });
        let src = self.s.source;
        let mut v: Vec<Task> =
            self.gw(read, src, format!("CHECKING {}", Self::dname(src)), geo).into_iter().collect();
        v.push(Task::CheckDone);
        self.launch(v);
    }

    fn directory(&mut self) {
        if self.eng.busy() || self.picking.is_some() {
            return;
        }
        let from_file = (self.s.section == Section::Copy && self.s.flow == Flow::AdfToDisk)
            || (self.s.section == Section::Tools && self.s.tool_mode.is_format() && self.s.tool_adf);
        if from_file {
            self.spawn_dialog(Pick::Dir, None);
            return;
        }
        if !self.source_ok() {
            return;
        }
        let temp = temp_path("dir.adf");
        let _ = std::fs::remove_file(&temp);
        self.temps.push(temp.clone());
        // Root block and directory blocks can be anywhere: read the whole disk, quickly.
        let read = Action::Read(ReadOpts {
            output: temp.clone(),
            format: self.s.sector_format().into(),
            profile: ReadProfile::QuickScan,
            ..Default::default()
        });
        let src = self.s.source;
        let mut v: Vec<Task> = self
            .gw(read, src, format!("READING {}", Self::dname(src)), TrackSet::dense(80, 2))
            .into_iter()
            .collect();
        v.push(Task::Dir { path: temp });
        v.push(Task::Done { message: "OPERATION DONE.".into() });
        self.launch(v);
    }

    fn disk_info(&mut self) {
        if self.eng.busy() || self.picking.is_some() {
            return;
        }
        let idx = if self.s.bus.drive(self.s.source).is_some() { self.s.source } else { 0 };
        let mut v: Vec<Task> = self
            .gw(
                Action::Info { bootloader: false },
                idx,
                "PROBING GREASEWEAZLE".into(),
                TrackSet::dense(80, 2),
            )
            .into_iter()
            .collect();
        v.push(Task::Info);
        self.launch(v);
    }

    // ───────────────────────────── requesters ─────────────────────────────

    fn open_setup(&mut self) {
        let mut m = Modal::new(
            ModalKind::Setup,
            "SETUP",
            vec![("Click BUS to change how the drives are wired.".into(), Level::Info)],
            vec!["DETECT", "OK", "CANCEL"],
        );
        m.fields = vec![
            Field { label: "GW PROGRAM (HOST TOOLS)", value: self.s.cfg.program.clone(), max: 400 },
            Field { label: "SERIAL PORT (EMPTY = AUTO)", value: self.s.cfg.device.clone(), max: 120 },
        ];
        m.bus = self.s.bus;
        m.sound = self.s.sound;
        self.modal = Some(m);
    }

    fn open_name(&mut self) {
        let mut m = Modal::new(
            ModalKind::Name,
            "VOLUME NAME",
            vec![("Name of the formatted disk (max 30 chars).".into(), Level::Info)],
            vec!["OK", "CANCEL"],
        );
        m.fields = vec![Field { label: "NAME", value: self.s.volume.clone(), max: 30 }];
        self.modal = Some(m);
    }

    fn open_log(&mut self) {
        let mut m = Modal::new(ModalKind::Log, "LOG", Vec::new(), vec!["OK", "CLEAR"]);
        m.scroll = usize::MAX;
        self.modal = Some(m);
    }

    fn modal_button(&mut self, i: usize) {
        let Some(m) = self.modal.as_mut() else { return };
        let label = m.buttons.get(i).copied().unwrap_or("");
        match m.kind {
            ModalKind::Engine => {
                // first button = OK / YES / REPLACE
                let yes = i == 0 && label != "CANCEL" && label != "NO";
                self.modal = None;
                self.eng.answer(yes);
            }
            ModalKind::Log => {
                if label == "CLEAR" {
                    self.eng.log.clear();
                } else {
                    self.modal = None;
                }
            }
            ModalKind::Name => {
                if label == "OK" {
                    let v = m.fields[0].value.trim().to_string();
                    match amigados::validate_name(&v) {
                        Ok(()) => {
                            self.s.volume = v;
                            self.modal = None;
                        }
                        Err(e) => m.error = Some(e.to_uppercase()),
                    }
                } else {
                    self.modal = None;
                }
            }
            ModalKind::Setup => match label {
                "DETECT" => {
                    let p = m.fields[0].value.trim().to_string();
                    m.error = Some("SEARCHING...".into());
                    self.s.cfg.program = if p.is_empty() { "gw".into() } else { p };
                    self.detect();
                }
                "OK" => {
                    let p = m.fields[0].value.trim().to_string();
                    let changed = p != self.s.cfg.program;
                    self.s.cfg.program = if p.is_empty() { "gw".into() } else { p };
                    self.s.cfg.device = m.fields[1].value.trim().to_string();
                    self.s.bus = m.bus;
                    self.s.sound = m.sound;
                    for i in 0..4 {
                        if self.s.bus.drive(i).is_none() {
                            self.s.targets[i] = false;
                        }
                    }
                    if self.s.bus.drive(self.s.source).is_none() {
                        self.s.source = 0;
                    }
                    if !self.s.targets.iter().any(|&t| t) {
                        self.s.targets[0] = true;
                    }
                    self.modal = None;
                    if changed {
                        self.detect();
                    }
                }
                _ => self.modal = None,
            },
        }
    }

    fn modal_keys(&mut self, ctx: &egui::Context) {
        let Some(m) = self.modal.as_mut() else { return };
        let events = ctx.input(|i| i.events.clone());
        let mut press: Option<usize> = None;
        for e in events {
            match e {
                egui::Event::Text(t) if !m.fields.is_empty() => {
                    let f = &mut m.fields[m.focus];
                    for ch in t.chars() {
                        if f.value.chars().count() < f.max && (ch as u32) >= 32 && (ch as u32) <= 255 {
                            f.value.push(ch);
                        }
                    }
                    m.error = None;
                }
                egui::Event::Paste(t) if !m.fields.is_empty() => {
                    let f = &mut m.fields[m.focus];
                    for ch in t.trim().chars() {
                        if f.value.chars().count() < f.max {
                            f.value.push(ch);
                        }
                    }
                }
                egui::Event::Key { key, pressed: true, .. } => match key {
                    Key::Backspace if !m.fields.is_empty() => {
                        m.fields[m.focus].value.pop();
                    }
                    Key::Tab if !m.fields.is_empty() => m.focus = (m.focus + 1) % m.fields.len(),
                    Key::Enter => {
                        press = Some(m.buttons.iter().position(|b| matches!(*b, "OK" | "YES")).unwrap_or(0));
                    }
                    Key::Escape => {
                        press = Some(
                            m.buttons
                                .iter()
                                .position(|b| matches!(*b, "CANCEL" | "NO"))
                                .unwrap_or(m.buttons.len().saturating_sub(1)),
                        );
                        if m.kind == ModalKind::Log {
                            press = Some(0);
                        }
                    }
                    Key::ArrowDown | Key::PageDown => {
                        m.scroll = m.scroll.saturating_add(if key == Key::PageDown { 10 } else { 1 })
                    }
                    Key::ArrowUp | Key::PageUp => {
                        m.scroll = m.scroll.saturating_sub(if key == Key::PageUp { 10 } else { 1 })
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        if let Some(i) = press {
            self.modal_button(i);
        }
    }

    fn apply(&mut self, act: Act) {
        let busy = self.eng.busy() || self.picking.is_some();
        let s = &mut self.s;
        match act {
            Act::Start => self.start(),
            Act::Repeat => self.repeat(),
            Act::Stop => self.eng.stop(),
            Act::DiskInfo => self.disk_info(),
            Act::Directory => self.directory(),
            Act::CheckDisk => self.check_disk(),
            Act::Setup => self.open_setup(),
            Act::Log => self.open_log(),
            Act::ModalButton(i) => self.modal_button(i),
            Act::ModalField(i) => {
                if let Some(m) = &mut self.modal {
                    m.focus = i;
                }
            }
            Act::ModalScroll(d) => {
                if let Some(m) = &mut self.modal {
                    m.scroll = if d < 0 {
                        m.scroll.saturating_sub((-d) as usize)
                    } else {
                        m.scroll.saturating_add(d as usize)
                    };
                }
            }
            Act::SoundToggle => {
                if let Some(m) = &mut self.modal {
                    m.sound = !m.sound;
                    if m.sound {
                        crate::sound::play(crate::sound::Sound::Done);
                    }
                }
            }
            Act::Bus => {
                if let Some(m) = &mut self.modal {
                    m.bus = if m.bus == Bus::Ibm { Bus::Shugart } else { Bus::Ibm };
                }
            }
            _ if busy => {}
            Act::Default => {
                s.defaults();
                self.eng.set_status("DEFAULT SETTINGS RESTORED.", Level::Good);
            }
            Act::Section(sec) => {
                if s.section == sec {
                    self.apply(Act::Mode(1));
                } else {
                    s.section = sec;
                }
            }
            Act::Mode(d) => match s.section {
                Section::Copy => {
                    let i = CopyMode::ALL.iter().position(|&m| m == s.copy_mode).unwrap_or(0) as i32;
                    s.copy_mode = CopyMode::ALL[(i + d).rem_euclid(CopyMode::ALL.len() as i32) as usize];
                }
                Section::Tools => {
                    let i = ToolMode::ALL.iter().position(|&m| m == s.tool_mode).unwrap_or(0) as i32;
                    s.tool_mode = ToolMode::ALL[(i + d).rem_euclid(ToolMode::ALL.len() as i32) as usize];
                }
            },
            Act::StartAdj(d) => s.start = (s.start as i32 + d).clamp(0, s.end as i32) as u16,
            Act::EndAdj(d) => s.end = (s.end as i32 + d).clamp(s.start as i32, MAX_CYL as i32) as u16,
            Act::Side => s.side = s.side.next(),
            Act::Dens => s.hd = !s.hd,
            Act::Flow => s.flow = s.flow.next(),
            Act::ToolDevice => s.tool_adf = !s.tool_adf,
            Act::Fs => s.ffs = !s.ffs,
            Act::EditName => self.open_name(),
            Act::Source(i) => {
                if s.bus.drive(i).is_some() {
                    s.source = i;
                }
            }
            Act::Target(i) => {
                if s.bus.drive(i).is_some() {
                    s.targets[i] = !s.targets[i];
                }
            }
        }
    }

    // ───────────────────────────── drawing ─────────────────────────────

    fn draw(&self, g: &Gfx, ui: &Ui, acts: &mut Vec<Act>, help: &mut Option<String>) {
        let live = self.modal.is_none();
        let busy = self.eng.busy() || self.picking.is_some();
        let hot =
            |id: &str, x: f32, y: f32, w: f32, h: f32| -> Hot { Hot::new(ui, live, id, g.c.r(x, y, w, h)) };

        g.fill(0.0, 0.0, gfx::W, gfx::H, pal::BG);

        // ── title band ──
        g.bead_frame(8.0, 8.0, 262.0, 182.0);
        g.text_centered(139.0, 36.0, 3.0, "\u{a9} 2026", Style::grad(pal::WHITE, pal::CYAN_DARK));
        g.text_centered(
            139.0,
            80.0,
            4.4,
            "RCC(A)",
            Style::grad(Color32::from_rgb(255, 244, 80), Color32::from_rgb(226, 36, 16)),
        );
        g.text_centered(139.0, 142.0, 2.0, "EDITION 1.0", Style::shadowed(pal::CYAN));

        g.fill(278.0, 8.0, 724.0, 182.0, pal::PANEL);
        g.dotted(282.0, 12.0, 716.0, 174.0);
        g.stroke(278.0, 8.0, 724.0, 182.0, 3.0, pal::LINE);
        gfx::logo(g, 640.0 - 6.0 * 9.0 * 12.0 / 2.0, 16.0, "X-COPY", 12.0);
        g.text2(
            640.0 - 20.0 * 8.0 * 3.2 / 2.0,
            124.0,
            3.2,
            4.4,
            "Retro Computing Club",
            Style::grad(Color32::from_rgb(240, 250, 255), Color32::from_rgb(60, 118, 146)),
        );
        g.text_centered(
            640.0,
            172.0,
            1.5,
            "MODERN  \u{b7}  GREASEWEAZLE POWERED",
            Style::flat(pal::CYAN_DARK),
        );

        g.bead_frame(1010.0, 8.0, 262.0, 182.0);
        let (gw_line, gw_level) = match (self.gw_found, self.eng.device_ok) {
            (None, _) => ("SEARCHING GW", Level::Warn),
            (Some(false), _) => ("GW NOT FOUND", Level::Error),
            (Some(true), Some(true)) => ("DEVICE OK", Level::Good),
            (Some(true), Some(false)) => ("NO DEVICE", Level::Error),
            (Some(true), None) => ("GW READY", Level::Good),
        };
        g.text_centered(1141.0, 34.0, 2.0, "GREASEWEAZLE", Style::shadowed(pal::CYAN));
        g.text_centered(1141.0, 60.0, 2.0, gw_line, Style::shadowed(level_color(gw_level)));
        let model: String = if self.eng.model.is_empty() {
            if self.s.bus == Bus::Ibm {
                "BUS: IBM PC".into()
            } else {
                "BUS: SHUGART".into()
            }
        } else {
            self.eng.model.to_uppercase().replace("GREASEWEAZLE ", "GW ").chars().take(13).collect()
        };
        g.text_centered(1141.0, 86.0, 2.0, &model, Style::shadowed(pal::CYAN));
        let h = hot("setup", 1050.0, 112.0, 182.0, 50.0);
        g.button(1050.0, 112.0, 182.0, 50.0, "SETUP", pal::YELLOW, h.hover, h.down);
        if h.hover {
            *help = Some("SETUP: GW PROGRAM, SERIAL PORT AND DRIVE BUS".into());
        }
        if h.click {
            acts.push(Act::Setup);
        }

        // ── option bar ──
        g.line_frame(8.0, 198.0, 1264.0, 120.0);
        g.fill(14.0, 240.0, 1252.0, 2.0, pal::LINE_DIM);
        let val = Style::grad(pal::YELLOW_TOP, pal::YELLOW_BOT);
        let dim = Style::shadowed(Color32::from_rgb(70, 76, 60));
        let vs = 4.0;
        let vy = 262.0;
        let header = |x: f32,
                      label: &str,
                      knob: bool,
                      active: bool,
                      id: &str,
                      a: Act,
                      tip: &str,
                      acts: &mut Vec<Act>,
                      help: &mut Option<String>| {
            let tw = crate::font::Font::width(label, 2.4) + if knob { 18.0 } else { 0.0 };
            let h = hot(id, x - 4.0, 208.0, tw + 8.0, 28.0);
            if knob {
                g.knob(x + 6.0, 221.0, h.hover);
            }
            let tx = if knob { x + 18.0 } else { x };
            let col = if active {
                pal::YELLOW
            } else if h.hover {
                pal::WHITE
            } else {
                pal::CYAN
            };
            g.text(tx, 212.0, 2.4, label, Style::shadowed(col));
            if knob && h.hover {
                *help = Some(tip.into());
            }
            if knob && h.click {
                acts.push(a);
            }
        };
        let copy = self.s.section == Section::Copy;
        header(
            24.0,
            "COPY",
            true,
            copy,
            "h-copy",
            Act::Section(Section::Copy),
            "COPY: CLICK TO CHOOSE THE COPY MODE",
            acts,
            help,
        );
        header(
            150.0,
            "TOOLS",
            true,
            !copy,
            "h-tools",
            Act::Section(Section::Tools),
            "TOOLS: FORMAT, QFORMAT, ERASE, SPEEDCHK, CLEAN",
            acts,
            help,
        );
        let mode_label = if copy { self.s.copy_mode.label() } else { self.s.tool_mode.label() };
        let h = hot("mode", 20.0, 252.0, 290.0, 52.0);
        g.text(28.0, vy, vs, mode_label, val);
        if h.hover {
            *help = Some(if copy { self.s.copy_mode.help() } else { self.s.tool_mode.help() }.into());
        }
        if h.click {
            acts.push(Act::Mode(1));
        }
        if h.rclick {
            acts.push(Act::Mode(-1));
        }

        let tool = self.s.tool_mode;
        let range_active = copy || tool == ToolMode::Erase;
        let format_tool = !copy && tool.is_format();
        let track0 = !copy && tool == ToolMode::Track0;
        // number with arrows
        let spin = |x: f32,
                    id: &str,
                    v: u16,
                    head: u8,
                    a: fn(i32) -> Act,
                    enabled: bool,
                    acts: &mut Vec<Act>,
                    help: &mut Option<String>| {
            let st = if enabled { val } else { dim };
            g.text(x, vy, vs, &format!("{v:02}"), st);
            g.text(x + 84.0, vy, vs, &head.to_string(), st);
            if !enabled {
                return;
            }
            let up = hot(&format!("{id}-up"), x + 8.0, 244.0, 48.0, 16.0);
            let dn = hot(&format!("{id}-dn"), x + 8.0, 296.0, 48.0, 16.0);
            g.arrow(x + 16.0, 252.0, true, up.hover);
            g.arrow(x + 48.0, 252.0, true, up.hover);
            g.arrow(x + 16.0, 304.0, false, dn.hover);
            g.arrow(x + 48.0, 304.0, false, dn.hover);
            let wheel = hot(&format!("{id}-num"), x, vy - 4.0, 100.0, 40.0);
            if up.hover || dn.hover || wheel.hover {
                *help = Some("CYLINDER: CLICK THE ARROWS (RIGHT-CLICK = 10 STEPS) OR SCROLL".into());
            }
            for (h, d) in [(&up, 1), (&dn, -1)] {
                if h.click {
                    acts.push(a(d));
                }
                if h.rclick {
                    acts.push(a(d * 10));
                }
            }
            if wheel.hover && wheel.scroll != 0 {
                acts.push(a(wheel.scroll));
            }
        };
        let cycle = |x: f32,
                     w: f32,
                     id: &str,
                     label: &str,
                     value: &str,
                     enabled: bool,
                     a: Act,
                     tip: &str,
                     acts: &mut Vec<Act>,
                     help: &mut Option<String>| {
            header(x, label, enabled, false, &format!("{id}-h"), a, tip, acts, help);
            g.text(x, vy, vs, value, if enabled { val } else { dim });
            if enabled {
                let h = hot(id, x - 4.0, 252.0, w, 52.0);
                if h.hover {
                    *help = Some(tip.into());
                }
                if h.click {
                    acts.push(a);
                }
            }
        };

        if track0 {
            header(330.0, "TRACK", false, false, "h-trk0", Act::Start, "", acts, help);
            g.text(330.0, vy, vs, "00", val);
            g.text(414.0, vy + 6.0, 3.0, "BOTH", val);
            cycle(
                640.0,
                190.0,
                "fs",
                "FS",
                if self.s.ffs { "FFS" } else { "OFS" },
                true,
                Act::Fs,
                "FS: MUST MATCH THE DISK (OFS = KICK 1.X, FFS = KICK 2.0+)",
                acts,
                help,
            );
        } else if format_tool {
            header(
                330.0,
                "NAME",
                true,
                false,
                "h-name",
                Act::EditName,
                "NAME: CLICK TO EDIT THE VOLUME NAME",
                acts,
                help,
            );
            let name: String = self.s.volume.to_uppercase().chars().take(11).collect();
            let more = if self.s.volume.chars().count() > 11 { "\u{bb}" } else { "" };
            g.text(330.0, vy + 6.0, 3.0, &format!("{name}{more}"), val);
            let h = hot("name", 326.0, 252.0, 300.0, 52.0);
            if h.hover {
                *help = Some("NAME: CLICK TO EDIT THE VOLUME NAME".into());
            }
            if h.click {
                acts.push(Act::EditName);
            }
            cycle(
                640.0,
                190.0,
                "fs",
                "FS",
                if self.s.ffs { "FFS" } else { "OFS" },
                true,
                Act::Fs,
                "FS: OFS = ALL AMIGAS (KICK 1.X), FFS = KICKSTART 2.0+",
                acts,
                help,
            );
        } else {
            header(330.0, "START", false, false, "h-start", Act::Start, "", acts, help);
            header(480.0, "END", false, false, "h-end", Act::Start, "", acts, help);
            spin(330.0, "start", self.s.start, self.s.start_head(), Act::StartAdj, range_active, acts, help);
            spin(480.0, "end", self.s.end, self.s.end_head(), Act::EndAdj, range_active, acts, help);
            cycle(
                640.0,
                190.0,
                "side",
                "SIDE",
                self.s.side.label(),
                range_active,
                Act::Side,
                "SIDE: BOTH, UPPER (HEAD 1) OR LOWER (HEAD 0)",
                acts,
                help,
            );
        }
        let dens_on = copy && self.s.copy_mode != CopyMode::Nibble || format_tool || track0;
        cycle(
            840.0,
            140.0,
            "dens",
            "DENS",
            if self.s.hd { "HD" } else { "DD" },
            dens_on,
            Act::Dens,
            "DENS: DD 880K OR HD 1.76M",
            acts,
            help,
        );
        if copy {
            cycle(
                990.0,
                260.0,
                "flow",
                "DEVICE",
                self.s.flow.label(),
                true,
                Act::Flow,
                self.s.flow.help(),
                acts,
                help,
            );
        } else if format_tool {
            let v = if self.s.tool_adf { "ADF" } else { "DISK" };
            cycle(
                990.0,
                260.0,
                "tdev",
                "DEVICE",
                v,
                true,
                Act::ToolDevice,
                "DEVICE: FORMAT THE DISK(S) OR SAVE A BLANK ADF FILE",
                acts,
                help,
            );
        } else {
            cycle(990.0, 260.0, "tdev", "DEVICE", "DISK", false, Act::ToolDevice, "", acts, help);
        }

        // ── buttons ──
        let bw = (1264.0 - 6.0 * 8.0) / 7.0;
        let buttons: [(&str, Color32, Act, &str); 7] = [
            ("START", pal::YELLOW, Act::Start, "START THE SELECTED OPERATION (ENTER)"),
            ("REPEAT", pal::YELLOW, Act::Repeat, "REPEAT: WRITE THE LAST IMAGE AGAIN TO NEW DISKS (F2)"),
            ("STOP", pal::YELLOW, Act::Stop, "STOP: INTERRUPT AND RELEASE THE DRIVE (ESC)"),
            ("DISK INFO", pal::GREEN, Act::DiskInfo, "DISK INFO: GREASEWEAZLE MODEL, FIRMWARE, PORT"),
            ("DIRECTORY", pal::GREEN, Act::Directory, "DIRECTORY: SHOW THE AMIGADOS FILES OF THE SOURCE"),
            ("CHECKDISK", pal::GREEN, Act::CheckDisk, "CHECKDISK: QUICK READ OF THE SOURCE, TRACK MAP"),
            ("DEFAULT", pal::RED, Act::Default, "DEFAULT: RESET ALL COPY OPTIONS"),
        ];
        for (i, (label, col, a, tip)) in buttons.iter().enumerate() {
            let x = 8.0 + i as f32 * (bw + 8.0);
            let h = hot(&format!("btn{i}"), x, 326.0, bw, 40.0);
            let enabled = match a {
                Act::Stop => busy,
                _ => !busy,
            };
            let c = if enabled { *col } else { lerp(*col, Color32::BLACK, 0.55) };
            g.button(x, 326.0, bw, 40.0, label, c, h.hover && enabled, h.down && enabled);
            if h.hover {
                *help = Some((*tip).into());
            }
            if h.click && enabled {
                acts.push(*a);
            }
        }

        // ── drives ──
        g.line_frame(8.0, 374.0, 500.0, 274.0);
        g.text_centered(258.0, 384.0, 2.4, "SOURCE DRIVES", Style::shadowed(pal::CYAN));
        g.text_centered(258.0, 626.0, 2.4, "TARGET DRIVES", Style::shadowed(pal::CYAN));
        let uses_source = (copy && self.s.flow != Flow::AdfToDisk) || (!copy && tool == ToolMode::Speed);
        let uses_target = (copy && self.s.flow != Flow::DiskToAdf)
            || (!copy && tool != ToolMode::Speed && !(format_tool && self.s.tool_adf));
        let blink = (g.t * 6.0).sin() > 0.0;
        for i in 0..4 {
            let x = 24.0 + i as f32 * 120.0;
            let enabled = self.s.bus.drive(i).is_some();
            let cx = x + 52.0;
            let hs = hot(&format!("src{i}"), cx - 20.0, 398.0, 40.0, 40.0);
            g.bulb(cx, 412.0, uses_source && self.s.source == i && enabled, enabled && uses_source);
            let ht = hot(&format!("tgt{i}"), cx - 20.0, 568.0, 40.0, 44.0);
            g.bulb(cx, 580.0, uses_target && self.s.targets[i] && enabled, enabled && uses_target);
            let led = match self.eng.active {
                Some((d, w)) if d == i && self.eng.running_gw() => Some(if w {
                    if blink {
                        pal::RED
                    } else {
                        pal::RED_DARK
                    }
                } else {
                    pal::GREEN
                }),
                _ => None,
            };
            let hd = hot(&format!("drv{i}"), x, 444.0, 104.0, 96.0);
            g.floppy(x, 444.0, i, enabled, led, hd.hover);
            if !enabled && (hs.hover || ht.hover || hd.hover) {
                *help = Some(format!("DF{i} IS NOT AVAILABLE ON THE IBM PC BUS (SEE SETUP)"));
            } else if hs.hover && uses_source {
                *help = Some(format!("SOURCE: READ FROM DF{i}"));
            } else if ht.hover && uses_target {
                *help = Some(format!("TARGET: CLICK TO ADD/REMOVE DF{i} (SEVERAL ALLOWED)"));
            } else if hd.hover {
                let d = self.s.bus.drive(i).map(|d| d.label()).unwrap_or("");
                *help = Some(format!("DF{i} = {}", d.to_uppercase()));
            }
            if hs.click && uses_source {
                acts.push(Act::Source(i));
            }
            if ht.click && uses_target {
                acts.push(Act::Target(i));
            }
        }

        // ── track maps ──
        let c = self.eng.map.counts();
        let footer_l = format!("OK:{:03} BAD:{:03}", c.ok + c.recovered, c.bad);
        let secs = self.eng.elapsed().as_secs();
        let footer_r = format!("TIME: {:02}:{:02}", secs / 60, secs % 60);
        self.grid(g, 516.0, "UPPER SIDE", 1, &footer_l, blink);
        self.grid(g, 898.0, "LOWER SIDE", 0, &footer_r, blink);

        // ── status line ──
        g.line_frame(8.0, 656.0, 1264.0, 56.0);
        let hs = hot("status", 8.0, 656.0, 1264.0, 56.0);
        if hs.click {
            acts.push(Act::Log);
        }
        let (text, lvl) = if !busy && live {
            match help {
                Some(h) => (h.clone(), Level::Cmd),
                None if hs.hover => ("CLICK HERE TO SHOW THE LOG.".to_string(), Level::Cmd),
                None => (self.eng.status.clone(), self.eng.level),
            }
        } else {
            (self.eng.status.clone(), self.eng.level)
        };
        let size = 3.0;
        let max = ((1264.0 - 40.0) / (8.0 * size)) as usize;
        let typed = if text == self.eng.status && !self.eng.running_gw() {
            ((self.status_since.elapsed().as_secs_f32() * 90.0) as usize).max(1)
        } else {
            usize::MAX
        };
        let shown: String = if text.chars().count() > max {
            let pad = format!("{text}     ");
            let n = pad.chars().count();
            let off = ((g.t * 8.0) as usize) % n;
            pad.chars().cycle().skip(off).take(max).collect()
        } else {
            text.chars().take(typed).collect()
        };
        let col = level_color(lvl);
        g.text(28.0, 672.0, size, &shown, Style::grad(lerp(col, Color32::WHITE, 0.35), col));
        if busy && self.eng.running_gw() {
            // progress bar under the text
            let p = self.eng.map.progress().clamp(0.0, 1.0);
            g.fill(28.0, 702.0, 1224.0, 3.0, pal::GREEN_DARK);
            g.fill(28.0, 702.0, 1224.0 * p, 3.0, pal::GREEN);
        }

        // tooltip of the hovered track cell
        if live {
            if let Some(pos) = ui.ctx().pointer_hover_pos() {
                let v = g.c.inv(pos);
                for (x0, head) in [(516.0, 1u8), (898.0, 0u8)] {
                    if let Some(cyl) = cell_at(x0, v.x, v.y) {
                        self.cell_tooltip(g, v.x, v.y, cyl, head);
                    }
                }
            }
        }
    }

    fn grid(&self, g: &Gfx, x: f32, title: &str, head: u8, footer: &str, blink: bool) {
        g.line_frame(x, 374.0, 374.0, 274.0);
        g.text_centered(x + 187.0, 384.0, 2.4, title, Style::shadowed(pal::CYAN));
        let (gx, gy, cw, ch) = grid_geo(x);
        for col in 0..10 {
            g.text_centered(
                gx + col as f32 * cw + cw / 2.0,
                406.0,
                2.0,
                &col.to_string(),
                Style::flat(pal::CYAN),
            );
        }
        let map = &self.eng.map;
        let in_head = map.geometry.heads.contains(&head);
        for row in 0..9u16 {
            g.text(x + 12.0, gy + row as f32 * ch + 4.0, 2.0, &row.to_string(), Style::flat(pal::CYAN));
            for col in 0..10u16 {
                let cyl = row * 10 + col;
                if cyl > MAX_CYL {
                    continue;
                }
                let (cx, cy) = (gx + col as f32 * cw, gy + row as f32 * ch);
                let inside = in_head && map.geometry.cyls.contains(&cyl);
                let info = map.get(cyl, head);
                let active = map.active == Some((cyl, head));
                let (bg, border, glyph, fg) =
                    cell_look(inside, info.state, info.sectors, active, blink, map.mode);
                g.fill(cx + 1.0, cy + 1.0, cw - 3.0, ch - 3.0, bg);
                g.stroke(cx + 1.0, cy + 1.0, cw - 3.0, ch - 3.0, 1.5, border);
                if !glyph.is_empty() {
                    g.text_centered(cx + cw / 2.0 - 1.0, cy + (ch - 16.0) / 2.0, 2.0, glyph, Style::flat(fg));
                }
            }
        }
        g.text_centered(x + 187.0, 624.0, 2.0, footer, Style::shadowed(pal::CYAN));
    }

    fn cell_tooltip(&self, g: &Gfx, mx: f32, my: f32, cyl: u16, head: u8) {
        let map = &self.eng.map;
        let info = map.get(cyl, head);
        let inside = map.geometry.heads.contains(&head) && map.geometry.cyls.contains(&cyl);
        let mut lines =
            vec![format!("TRACK {cyl:02} SIDE {head} ({})", if head == 1 { "UPPER" } else { "LOWER" })];
        if inside {
            lines.push(info.state.label().to_uppercase());
            if let Some((ok, tot)) = info.sectors {
                lines.push(format!("SECTORS {ok}/{tot}"));
            }
            if info.retries > 0 {
                lines.push(format!("RETRIES {}", info.retries));
            }
            if !info.sector_map.is_empty() {
                let s: String = info
                    .sector_map
                    .iter()
                    .map(|s| match s {
                        Some(true) => 'o',
                        Some(false) => 'X',
                        None => '.',
                    })
                    .collect();
                lines.push(s);
            }
        } else {
            lines.push("NOT IN THIS OPERATION".into());
        }
        let w = lines.iter().map(|l| l.len()).max().unwrap_or(0) as f32 * 12.8 + 20.0;
        let h = lines.len() as f32 * 16.0 + 14.0;
        let x = (mx + 18.0).min(gfx::W - w - 6.0);
        let y = (my - h - 8.0).max(4.0);
        g.fill(x, y, w, h, Color32::from_rgb(4, 6, 14));
        g.stroke(x, y, w, h, 2.0, pal::YELLOW);
        for (i, l) in lines.iter().enumerate() {
            g.text(
                x + 10.0,
                y + 8.0 + i as f32 * 16.0,
                1.6,
                l,
                Style::flat(if i == 0 { pal::YELLOW } else { pal::CYAN }),
            );
        }
    }

    fn draw_modal(&self, g: &Gfx, ui: &Ui, acts: &mut Vec<Act>) {
        let Some(m) = &self.modal else { return };
        let hot =
            |id: &str, x: f32, y: f32, w: f32, h: f32| -> Hot { Hot::new(ui, true, id, g.c.r(x, y, w, h)) };
        // swallow clicks on the background
        let _ = hot("modal-bg", 0.0, 0.0, gfx::W, gfx::H);
        g.p.rect_filled(g.c.r(0.0, 0.0, gfx::W, gfx::H), 0.0, Color32::from_rgba_unmultiplied(0, 0, 0, 170));

        let log = m.kind == ModalKind::Log;
        let size = if log { 1.25 } else { 2.2 };
        let wbox = if log { 1100.0 } else { 820.0 };
        let cols = ((wbox - 64.0) / (8.0 * size)) as usize;
        let mut lines: Vec<(String, Level)> = Vec::new();
        let src: Vec<(String, Level)> = if log { self.eng.log.clone() } else { m.lines.clone() };
        for (l, lv) in &src {
            for w in wrap(l, cols) {
                lines.push((w, *lv));
            }
        }
        let visible = if log { 30 } else { 13 };
        let max_scroll = lines.len().saturating_sub(visible);
        let scroll = m.scroll.min(max_scroll);
        let shown = lines.len().min(visible);
        let lh = 8.0 * size + 6.0;
        let fields_h =
            m.fields.len() as f32 * 64.0 + 8.0 + if m.kind == ModalKind::Setup { 72.0 } else { 0.0 };
        let err_h = if m.error.is_some() { 28.0 } else { 0.0 };
        let hbox = 96.0 + shown as f32 * lh + fields_h + err_h + 70.0;
        let x = (gfx::W - wbox) / 2.0;
        let y = ((gfx::H - hbox) / 2.0).max(4.0);
        g.bead_frame(x, y, wbox, hbox);
        g.text_centered(
            x + wbox / 2.0,
            y + 30.0,
            3.0,
            &m.title,
            Style::grad(pal::YELLOW_TOP, pal::YELLOW_BOT),
        );
        let mut cy = y + 70.0;
        for (l, lv) in lines.iter().skip(scroll).take(visible) {
            g.text(x + 32.0, cy, size, l, Style::shadowed(level_color(*lv)));
            cy += lh;
        }
        if max_scroll > 0 {
            let track_h = shown as f32 * lh;
            let bar_h = (track_h * visible as f32 / lines.len() as f32).max(12.0);
            let by = y + 70.0 + (track_h - bar_h) * scroll as f32 / max_scroll as f32;
            g.fill(x + wbox - 28.0, y + 70.0, 6.0, track_h, pal::LINE_DIM);
            g.fill(x + wbox - 28.0, by, 6.0, bar_h, pal::CYAN);
            let area = hot("modal-scroll", x, y + 60.0, wbox, track_h + 20.0);
            if area.scroll != 0 {
                acts.push(Act::ModalScroll(-area.scroll * 3));
            }
        }
        if !m.fields.is_empty() {
            cy += 8.0;
        }
        for (i, f) in m.fields.iter().enumerate() {
            g.text(x + 32.0, cy + 4.0, 2.0, f.label, Style::shadowed(pal::CYAN));
            let (fx, fy, fw, fh) = (x + 32.0, cy + 24.0, wbox - 64.0, 32.0);
            let h = hot(&format!("field{i}"), fx, fy, fw, fh);
            g.fill(fx, fy, fw, fh, Color32::from_rgb(2, 4, 10));
            g.stroke(fx, fy, fw, fh, 2.0, if i == m.focus { pal::YELLOW } else { pal::LINE_DIM });
            let maxc = ((fw - 20.0) / 16.0) as usize - 1;
            let n = f.value.chars().count();
            let tail: String = f.value.chars().skip(n.saturating_sub(maxc)).collect();
            let cursor = if i == m.focus && (g.t * 2.5).fract() < 0.6 { "_" } else { "" };
            g.text(fx + 10.0, fy + 8.0, 2.0, &format!("{tail}{cursor}"), Style::flat(pal::YELLOW));
            if h.click {
                acts.push(Act::ModalField(i));
            }
            cy += 64.0;
        }
        if m.kind == ModalKind::Setup {
            let h = hot("bus", x + 32.0, cy, wbox - 64.0, 28.0);
            g.knob(x + 40.0, cy + 12.0, h.hover);
            g.text(
                x + 56.0,
                cy + 4.0,
                2.0,
                &format!("BUS: {}", m.bus.label()),
                Style::shadowed(if h.hover { pal::WHITE } else { pal::YELLOW }),
            );
            if h.click {
                acts.push(Act::Bus);
            }
            cy += 36.0;
            let h = hot("sound", x + 32.0, cy, wbox - 64.0, 28.0);
            g.knob(x + 40.0, cy + 12.0, h.hover);
            g.text(
                x + 56.0,
                cy + 4.0,
                2.0,
                &format!("SOUND: {}", if m.sound { "ON" } else { "OFF" }),
                Style::shadowed(if h.hover { pal::WHITE } else { pal::YELLOW }),
            );
            if h.click {
                acts.push(Act::SoundToggle);
            }
            cy += 36.0;
        }
        if let Some(e) = &m.error {
            g.text(x + 32.0, cy + 2.0, 2.0, e, Style::shadowed(pal::RED));
        }
        let n = m.buttons.len() as f32;
        let bw = 170.0;
        let gap = 24.0;
        let total = n * bw + (n - 1.0) * gap;
        let by = y + hbox - 60.0;
        for (i, b) in m.buttons.iter().enumerate() {
            let bx = x + (wbox - total) / 2.0 + i as f32 * (bw + gap);
            let h = hot(&format!("mbtn{i}"), bx, by, bw, 38.0);
            let col = match *b {
                "NO" | "CANCEL" => pal::RED,
                "DETECT" | "CLEAR" => pal::GREEN,
                _ => pal::YELLOW,
            };
            g.button(bx, by, bw, 38.0, b, col, h.hover, h.down);
            if h.click {
                acts.push(Act::ModalButton(i));
            }
        }
    }
}

/// Grid origin and cell size of a track map panel at `x`.
fn grid_geo(x: f32) -> (f32, f32, f32, f32) {
    (x + 34.0, 424.0, 33.0, 21.5)
}

fn cell_at(x0: f32, vx: f32, vy: f32) -> Option<u16> {
    let (gx, gy, cw, ch) = grid_geo(x0);
    if vx < gx || vy < gy || vx >= gx + cw * 10.0 || vy >= gy + ch * 9.0 {
        return None;
    }
    let cyl = ((vy - gy) / ch) as u16 * 10 + ((vx - gx) / cw) as u16;
    (cyl <= MAX_CYL).then_some(cyl)
}

/// Colours and letter of one track cell.
fn cell_look(
    inside: bool,
    state: TrackState,
    sectors: Option<(u32, u32)>,
    active: bool,
    blink: bool,
    mode: hgf_core::MapMode,
) -> (Color32, Color32, &'static str, Color32) {
    const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
    let empty_bg = Color32::from_rgb(6, 16, 12);
    if !inside {
        return (empty_bg, Color32::from_rgb(16, 46, 26), "", pal::GREY);
    }
    if active {
        return if blink {
            (pal::WHITE, pal::WHITE, if mode == hgf_core::MapMode::Read { "R" } else { "W" }, Color32::BLACK)
        } else {
            (pal::GREEN_BOX, pal::WHITE, "", pal::WHITE)
        };
    }
    match state {
        TrackState::Pending => (empty_bg, pal::GREEN_BOX, "", pal::GREEN),
        TrackState::Active => (pal::WHITE, pal::WHITE, "", Color32::BLACK),
        TrackState::Ok => {
            let g = if mode == hgf_core::MapMode::Erase { "E" } else { "O" };
            (pal::GREEN_DARK, pal::GREEN, g, pal::GREEN)
        }
        TrackState::Recovered => (Color32::from_rgb(70, 60, 0), pal::YELLOW, "R", pal::YELLOW),
        TrackState::Retrying => {
            let c = if blink { pal::ORANGE } else { Color32::from_rgb(90, 50, 0) };
            (Color32::from_rgb(40, 24, 0), c, "?", pal::ORANGE)
        }
        TrackState::Bad => {
            let missing = sectors.map(|(ok, tot)| tot.saturating_sub(ok)).unwrap_or(0);
            let g = if (1..=9).contains(&missing) { DIGITS[missing as usize] } else { "X" };
            (pal::RED_DARK, pal::RED, g, Color32::from_rgb(255, 210, 200))
        }
        TrackState::Empty => {
            (Color32::from_rgb(10, 20, 50), pal::BLUE, "U", Color32::from_rgb(140, 170, 255))
        }
        TrackState::Flux => (Color32::from_rgb(0, 40, 50), pal::CYAN, "F", pal::CYAN),
        TrackState::OutOfRange => (empty_bg, pal::LINE_DIM, "-", pal::GREY),
    }
}

/// Interaction state of a hotspot.
#[derive(Default)]
struct Hot {
    hover: bool,
    click: bool,
    rclick: bool,
    down: bool,
    scroll: i32,
}

impl Hot {
    fn new(ui: &Ui, live: bool, id: &str, r: Rect) -> Self {
        if !live {
            return Self::default();
        }
        let resp = ui.interact(r, Id::new(("xc", id)), Sense::click());
        let hover = resp.hovered();
        if hover {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        let scroll = if hover {
            let dy: f32 = ui.ctx().input(|i| {
                i.events
                    .iter()
                    .map(|e| match e {
                        egui::Event::MouseWheel { delta, .. } => delta.y,
                        _ => 0.0,
                    })
                    .sum()
            });
            if dy > 0.5 {
                1
            } else if dy < -0.5 {
                -1
            } else {
                0
            }
        } else {
            0
        };
        Self {
            hover,
            click: resp.clicked(),
            rclick: resp.secondary_clicked(),
            down: resp.is_pointer_button_down_on(),
            scroll,
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_detect();
        self.poll_pick();
        let animating = self.eng.tick();
        if let Some(snd) = self.eng.sound.take() {
            if self.s.sound {
                crate::sound::play(snd);
            }
        }
        if let Some(name) = self.eng.probe_name.take() {
            // DF>ADF: the volume name is known, now ask where to save the image.
            if self.s.section == Section::Copy && self.s.flow == Flow::DiskToAdf {
                let nibble = self.s.copy_mode == CopyMode::Nibble;
                let ext = if nibble { "scp" } else { "adf" };
                let stem = name.as_deref().map(amiga::file_stem).unwrap_or_else(|| "disk".into());
                let what = if nibble { "SCP flux image" } else { "Amiga disk file" };
                self.spawn_dialog(Pick::ReadTo, Some((format!("{stem}.{ext}"), ext, what)));
            }
        }
        if self.modal.is_none() {
            if let Some(a) = self.eng.ask.clone() {
                let buttons = if let Some((y, n)) = a.choice {
                    vec![y, n]
                } else if a.notice {
                    vec!["OK"]
                } else if a.yes_no {
                    vec!["YES", "NO"]
                } else {
                    vec!["OK", "CANCEL"]
                };
                self.modal = Some(Modal::new(ModalKind::Engine, &a.title, a.lines, buttons));
            }
        }
        if let Some((n, _)) = &self.pending_image {
            if self.eng.done_count >= *n {
                self.last_image = self.pending_image.take().map(|p| p.1);
            } else if !self.eng.busy() {
                self.pending_image = None;
            }
        }
        if self.eng.status != self.status_seen {
            self.status_seen = self.eng.status.clone();
            self.status_since = Instant::now();
        }

        // keyboard
        if self.modal.is_some() {
            self.modal_keys(&ctx);
        } else {
            let (enter, esc, f2) = ctx
                .input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape), i.key_pressed(Key::F2)));
            if esc {
                self.eng.stop();
            } else if enter {
                self.start();
            } else if f2 {
                self.repeat();
            }
        }

        let screen = ui.max_rect();
        if screen.width() < 2.0 || screen.height() < 2.0 {
            // minimised: nothing to draw
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
            return;
        }
        let painter = ui.painter().clone();
        painter.rect_filled(screen, 0.0, pal::BG);
        let mut acts = Vec::new();
        let mut help = None;
        {
            let g = Gfx {
                p: painter,
                c: Canvas::fit(screen),
                font: &self.font,
                dots: &self.dots,
                t: self.t0.elapsed().as_secs_f64(),
            };
            self.draw(&g, ui, &mut acts, &mut help);
            self.draw_modal(&g, ui, &mut acts);
        }
        self.help = help;
        for a in acts {
            self.apply(a);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(if animating || self.modal.is_some() {
            33
        } else {
            80
        }));
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, STORAGE_KEY, &self.s);
    }

    fn on_exit(&mut self) {
        crate::sound::cleanup();
        self.eng.stop();
        for t in &self.temps {
            let _ = std::fs::remove_file(t);
        }
    }
}
