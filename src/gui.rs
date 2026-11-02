//! Native GUI window (egui + glow/OpenGL). This is the default entry
//! point on Windows: double-click -> real window, no browser, no web page.
//!
//! Layout follows the FlClash-style redesign (see `../ui-mockup/PROPOSAL.md`):
//! left nav sidebar, central icon-card grid, right detail card, bottom
//! status bar. The heavy lifting is `pipeline.rs`, shared with the Web UI.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use egui::{ColorImage, TextureHandle, TextureOptions};

use crate::editing::{EditState, Feedback, History, StudioSession};
use crate::icon_pack::{PackPlan, Quality};
use crate::itunes::{self, AppCandidate};
use crate::lnk::{self, ShortcutInfo};
use crate::pipeline::{self, ApplyItem, ApplyResult, SourceSpec};
use crate::presets::StylePreset;
use crate::server::AppConfig;
use crate::ui_icons::{draw_icon, Icon};

/// 窗口 / 任务栏图标(编译时内嵌 256px PNG)。
fn app_icon() -> egui::IconData {
    let bytes = include_bytes!("../assets/icon_256.png");
    match image::load_from_memory(bytes) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (width, height) = rgba.dimensions();
            egui::IconData {
                rgba: rgba.into_raw(),
                width,
                height,
            }
        }
        Err(_) => egui::IconData::default(),
    }
}

pub fn run() -> anyhow::Result<()> {
    run_with_resume(None)
}

pub fn run_with_resume(resume: Option<crate::elevation::ResumeState>) -> anyhow::Result<()> {
    // The exe is a console-subsystem binary (so the CLI subcommands keep
    // their console). Detach the console so a double-clicked GUI launch
    // doesn't leave a stray black window behind. Harmless when there is
    // no console to detach from; CLI paths never call this.
    unsafe {
        let _ = windows::Win32::System::Console::FreeConsole();
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 640.0])
            .with_min_inner_size([480.0, 560.0])
            .with_drag_and_drop(true)
            .with_icon(app_icon())
            .with_title(format!("IconForge 图标工坊 v{}", env!("CARGO_PKG_VERSION"))),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "IconForge 图标工坊",
        options,
        Box::new(move |cc| {
            let mut app = App::new(cc);
            if let Some(state) = resume {
                app.restore_resume(&cc.egui_ctx, state);
            }
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|e| anyhow::anyhow!("GUI 启动失败: {e}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceTab {
    Itunes,
    Current,
    File,
}

type SearchReply = (String, Result<Vec<(u32, AppCandidate)>, String>);
type PreviewReply = Result<(image::RgbaImage, String), String>;
type ApplyReply = (Vec<ApplyResult>, Option<String>);

enum AssetReply {
    ResourcePage(crate::resources::Page),
    ResourceChosen(PathBuf),
    ThemeLoaded(crate::theme_bundle::Theme),
    ThemeInstalled(PathBuf),
    ThemeExported(PathBuf, usize, Vec<String>),
}
struct ResourcePicker {
    file: PathBuf,
    total: u32,
    start: u32,
    icons: Vec<(i32, TextureHandle)>,
}
enum DavAction {
    Test,
    Upload,
    Download,
    Restore,
}
enum DavReply {
    Message(String),
    Downloaded(crate::webdav::Downloaded),
    Restored(String),
}
type DavResult = Result<DavReply, String>;
enum RecoveryEvent {
    Progress(crate::recovery::Progress),
    Finished(Result<crate::recovery::Report, String>),
}

struct App {
    folders: Vec<(String, PathBuf)>,
    folder_idx: usize,
    shortcuts: Vec<ShortcutInfo>,
    icons: Vec<Option<TextureHandle>>,
    filter: String,
    selected: Option<usize>,
    before: Option<TextureHandle>,
    after: Option<TextureHandle>,
    branch: String,
    base: String,
    /// Custom base-plate color (GUI color picker). Encoded into `base`
    /// as `custom:#RRGGBB` when the custom base is active; restored
    /// from the applied cache on select.
    custom_base: crate::gradient::Rgb,
    gradient_draft: crate::gradient::LinearGradient,
    asset_rx: Option<std::sync::mpsc::Receiver<Result<AssetReply, String>>>,
    asset_writing: bool,
    resource_picker: Option<ResourcePicker>,
    theme_name: String,
    theme_pending: Option<crate::theme_bundle::Theme>,
    theme_import_styles: bool,
    /// Crop-to-zoom glyph scale (0.5–1.2, default 0.76). Rounded-corner
    /// icons look a ring smaller than full-bleed ones; the slider zooms
    /// them to a suitable size. Per-icon: restored from the applied
    /// cache on select, default 0.76 for fresh icons.
    glyph_scale: f32,
    /// LinkEcho-basis "图标圆角": glyph corner radius (0–0.5,
    /// fraction of glyph max side). Per-icon, restored from cache.
    icon_radius: f32,
    /// LinkEcho-basis "底板大小": base-plate size (0.6–1.0,
    /// fraction of canvas). Per-icon, restored from cache.
    base_size: f32,
    /// LinkEcho-basis "底板圆角": base-plate roundness slider
    /// (0–100%: 0 = square, 100 = round; maps to superellipse
    /// exponent 8.0–2.0). Per-icon, restored from cache.
    base_radius: f32,
    tab: SourceTab,
    search_query: String,
    /// "lnk_path\nquery" of the last successful iTunes search: auto-search
    /// skips re-running it when switching tabs back and forth.
    searched_for: Option<String>,
    candidates: Vec<(u32, AppCandidate)>,
    cand_selected: Option<usize>,
    /// Candidate artwork thumbnails, keyed by thumbnail URL. Fetched on
    /// a background thread (never block the UI on thumbnail downloads);
    /// the receiver is drained every frame in ui().
    cand_textures: HashMap<String, TextureHandle>,
    artwork_rx: Option<std::sync::mpsc::Receiver<(String, Vec<u8>)>>,
    search_rx: Option<std::sync::mpsc::Receiver<SearchReply>>,
    preview_rx: Option<std::sync::mpsc::Receiver<PreviewReply>>,
    preview_ticket: std::sync::Arc<std::sync::atomic::AtomicU64>,
    apply_rx: Option<std::sync::mpsc::Receiver<ApplyReply>>,
    recovery_rx: Option<std::sync::mpsc::Receiver<RecoveryEvent>>,
    recovery_label: String,
    recovery_progress: Option<(usize, usize)>,
    file_path: String,
    file_design: bool,
    studio_file: String,
    studio_design: bool,
    edit_history: History,
    history_scope: Option<String>,
    studio_session: StudioSession,
    session_dirty: Option<std::time::Instant>,
    session_read_error: bool,
    status_seen: String,
    status_since: std::time::Instant,
    preview_master: Option<image::RgbaImage>,
    export_rx: Option<std::sync::mpsc::Receiver<Result<PathBuf, String>>>,
    pending_image: Option<PathBuf>,
    style_presets: Vec<StylePreset>,
    draft_style: Option<StylePreset>,
    preset_error: String,
    delete_preset: Option<String>,
    pack_plan: Option<PackPlan>,
    pack_selected: Option<usize>,
    pack_scan_rx: Option<std::sync::mpsc::Receiver<Result<PackPlan, String>>>,
    pack_preview_rx: Option<std::sync::mpsc::Receiver<PreviewReply>>,
    pack_before: Option<TextureHandle>,
    pack_after: Option<TextureHandle>,
    /// Oldest recorded original icon per shortcut (pipeline::oldest_originals).
    /// Feeds the LinkEcho-basis Changed rule; stable across apply/restore
    /// because the oldest original never changes.
    originals: HashMap<String, String>,
    /// Applied-icon cache (pipeline::applied_cache_load): shortcut ->
    /// the ICO it was last applied with. Powers the 重新应用 button.
    applied: HashMap<String, pipeline::AppliedEntry>,
    excluded: HashSet<String>,
    status: String,
    active_page: usize,
    editing: bool,
    /// LinkEcho-basis Log window: action history (apply/restore/
    /// folder switches), newest last, capped at 200 lines.
    show_log: bool,
    log: Vec<String>,
    webdav_settings: crate::webdav::Settings,
    webdav_status: String,
    webdav_rx: Option<std::sync::mpsc::Receiver<DavResult>>,
    webdav_mutating: bool,
    webdav_download: Option<crate::webdav::Downloaded>,
    snapshots: Vec<pipeline::SnapshotRow>,
    small_preview_textures: Vec<(u32, TextureHandle)>,
    target_checks: HashMap<String, crate::maintenance::TargetHealth>,
    target_check_rx:
        Option<std::sync::mpsc::Receiver<HashMap<String, crate::maintenance::TargetHealth>>>,
    only_invalid: bool,
    backup_inventory: Option<crate::maintenance::Inventory>,
    backup_rx: Option<
        std::sync::mpsc::Receiver<(
            Result<crate::maintenance::Inventory, String>,
            Option<String>,
        )>,
    >,
    backup_deleting: bool,
    backup_selected: HashSet<String>,
    backup_confirm: Option<Vec<String>>,
    selected_snaps: HashSet<String>,
    /// Current folder is protected (Public Desktop/Start Menu/ProgramData)
    /// or the last apply hit access denied: writes need elevation.
    needs_admin: bool,
    /// Dark theme toggle (sidebar switch, persisted in config.json).
    dark_mode: bool,
    show_checkerboard: bool,
    /// This process already runs with an elevated token.
    is_admin: bool,
    folder_rx: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    admin_rx: Option<std::sync::mpsc::Receiver<Result<crate::elevation::Outcome, String>>>,
    /// UI zoom factor (Ctrl+= / Ctrl+- / Ctrl+0 / Ctrl+wheel, persisted).
    ui_scale: f32,
}

impl App {
    fn invalidate_reads(&mut self) {
        self.search_rx = None;
        self.preview_rx = None;
        self.preview_master = None;
        self.small_preview_textures.clear();
        self.artwork_rx = None;
        self.pack_scan_rx = None;
        self.pack_preview_rx = None;
        self.preview_ticket
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn poll_jobs(&mut self, ctx: &egui::Context) {
        if let Some(rx) = self.search_rx.take() {
            match rx.try_recv() {
                Ok((key, result)) => self.finish_search(ctx, key, result),
                Err(std::sync::mpsc::TryRecvError::Empty) => self.search_rx = Some(rx),
                Err(_) => self.status = "搜索任务异常结束，请重试".into(),
            }
        }
        if let Some(rx) = self.preview_rx.take() {
            match rx.try_recv() {
                Ok(result) => self.finish_preview(ctx, result),
                Err(std::sync::mpsc::TryRecvError::Empty) => self.preview_rx = Some(rx),
                Err(_) => self.status = "预览任务异常结束，请重试".into(),
            }
        }
        if let Some(rx) = self.apply_rx.take() {
            match rx.try_recv() {
                Ok((results, snapshot)) => self.finish_apply(ctx, results, snapshot),
                Err(std::sync::mpsc::TryRecvError::Empty) => self.apply_rx = Some(rx),
                Err(_) => self.status = "应用任务异常结束，请检查快照和日志".into(),
            }
        }
        if let Some(rx) = self.webdav_rx.take() {
            match rx.try_recv() {
                Ok(result) => {
                    self.webdav_mutating = false;
                    match result {
                        Ok(DavReply::Message(msg)) => self.webdav_status = msg,
                        Ok(DavReply::Downloaded(backup)) => {
                            self.webdav_status = "下载完成，请核对下方内容后确认恢复".into();
                            self.webdav_download = Some(backup);
                        }
                        Ok(DavReply::Restored(msg)) => {
                            self.webdav_status = msg;
                            self.applied = pipeline::applied_cache_load();
                            self.snapshots = pipeline::list_snapshots();
                            let cfg = AppConfig::load();
                            self.dark_mode = cfg.dark_mode;
                            self.style_presets = cfg.style_presets.clone();
                            self.excluded = cfg
                                .excluded_shortcuts
                                .iter()
                                .map(|p| lnk::path_key(p))
                                .collect();
                            if (UI_SCALE_MIN..=UI_SCALE_MAX).contains(&cfg.ui_scale) {
                                ctx.set_zoom_factor(cfg.ui_scale);
                                self.ui_scale = cfg.ui_scale;
                            }
                            self.reload_shortcuts(ctx.clone());
                        }
                        Err(error) => self.webdav_status = format!("WebDAV 失败：{error}"),
                    }
                    self.status = self.webdav_status.clone();
                    self.log_push(&self.webdav_status.clone());
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => self.webdav_rx = Some(rx),
                Err(_) => {
                    self.webdav_mutating = false;
                    self.webdav_status = "WebDAV 任务异常结束，请重试".into();
                    self.status = self.webdav_status.clone();
                }
            }
        }
        if let Some(rx) = self.pack_scan_rx.take() {
            match rx.try_recv() {
                Ok(Ok(plan)) => {
                    self.status = format!(
                        "图标包：{} 个文件，匹配 {} 项，跳过损坏文件 {} 个",
                        plan.file_count,
                        plan.matched_count(),
                        plan.ignored_count
                    );
                    self.pack_plan = Some(plan);
                    self.pack_selected = None;
                    self.active_page = 6;
                    self.editing = false;
                }
                Ok(Err(error)) => self.status = format!("读取图标包失败：{error}"),
                Err(std::sync::mpsc::TryRecvError::Empty) => self.pack_scan_rx = Some(rx),
                Err(_) => self.status = "图标包读取任务异常结束".into(),
            }
        }
        if let Some(rx) = self.pack_preview_rx.take() {
            match rx.try_recv() {
                Ok(Ok((image, _))) => {
                    self.pack_after = Some(texture_from_image(ctx.clone(), "pack-preview", &image));
                    self.status = "匹配预览就绪".into();
                }
                Ok(Err(error)) => {
                    self.pack_after = None;
                    self.status = format!("匹配预览失败：{error}");
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => self.pack_preview_rx = Some(rx),
                Err(_) => self.status = "匹配预览任务异常结束".into(),
            }
        }
        if self.search_rx.is_some()
            || self.preview_rx.is_some()
            || self.apply_rx.is_some()
            || self.webdav_rx.is_some()
            || self.pack_scan_rx.is_some()
            || self.pack_preview_rx.is_some()
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn mutation_busy(&self) -> bool {
        self.apply_rx.is_some()
            || self.recovery_rx.is_some()
            || self.webdav_mutating
            || self.admin_rx.is_some()
            || self.export_rx.is_some()
            || self.backup_deleting
            || self.asset_writing
    }

    fn start_webdav(&mut self, ctx: &egui::Context, action: DavAction) {
        if self.webdav_rx.is_some() || self.mutation_busy() {
            return;
        }
        let backup = if matches!(action, DavAction::Restore) {
            let Some(backup) = self.webdav_download.take() else {
                return;
            };
            Some(backup)
        } else {
            None
        };
        self.webdav_mutating = matches!(action, DavAction::Upload | DavAction::Restore);
        if self.webdav_mutating {
            self.invalidate_reads();
        }
        self.webdav_status = match action {
            DavAction::Test => "正在测试连接…",
            DavAction::Upload => "正在打包并上传备份…",
            DavAction::Download => "正在下载并校验云端备份…",
            DavAction::Restore => "正在保存本地副本并恢复数据…",
        }
        .into();
        self.status = self.webdav_status.clone();
        let settings = self.webdav_settings.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.webdav_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = match action {
                DavAction::Test => crate::webdav::test_connection(&settings).map(DavReply::Message),
                DavAction::Upload => crate::webdav::upload(&settings).map(DavReply::Message),
                DavAction::Download => crate::webdav::download(&settings).map(DavReply::Downloaded),
                DavAction::Restore => {
                    crate::webdav::restore(backup.unwrap()).map(DavReply::Restored)
                }
            }
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    fn webdav_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::ScrollArea::vertical().max_height(560.0).show(ui, |ui| {
            ui.add_space(8.0);
            let busy = self.webdav_rx.is_some() || self.mutation_busy();
            ui.add_enabled_ui(!busy, |ui| {
                ui.label("WebDAV 地址");
                ui.add(egui::TextEdit::singleline(&mut self.webdav_settings.url)
                    .hint_text("https://server.example/dav/").desired_width(f32::INFINITY));
                if self.webdav_settings.url.trim().starts_with("http://") {
                    ui.label("HTTP 连接不加密，建议使用 HTTPS。");
                }
                ui.label("账号");
                ui.add(egui::TextEdit::singleline(&mut self.webdav_settings.username).desired_width(f32::INFINITY));
                ui.label("密码 / 应用专用密码");
                ui.add(egui::TextEdit::singleline(&mut self.webdav_settings.password).password(true).desired_width(f32::INFINITY));
                ui.label("密码仅加密保存在本机");
                ui.horizontal_wrapped(|ui| {
                    if ui.button("保存配置").clicked() {
                        self.webdav_status = match self.webdav_settings.save() {
                            Ok(()) => "WebDAV 配置已保存".into(),
                            Err(e) => format!("保存失败：{e:#}"),
                        };
                    }
                    if ui.button("测试连接").clicked() { self.start_webdav(ctx, DavAction::Test); }
                });
            });
            ui.add_space(8.0);
            ui.separator();
            ui.label(egui::RichText::new("备份目录：IconForge/").small());
            ui.add_enabled_ui(!busy, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui.button("上传本机备份").clicked() { self.start_webdav(ctx, DavAction::Upload); }
                    if ui.button("下载最新备份").clicked() {
                        self.webdav_download = None;
                        self.start_webdav(ctx, DavAction::Download);
                    }
                });
            });
            if self.webdav_rx.is_some() { ui.spinner(); }
            ui.label(&self.webdav_status);
            if let Some(backup) = &self.webdav_download {
                ui.separator();
                ui.label(format!("待恢复：{} 个文件，约 {:.1} KiB", backup.file_count, backup.total_bytes as f64 / 1024.0));
                ui.label(format!("来源数据目录：{}", backup.source_root));
                ui.label("确认后合并到本机，同名数据会被覆盖；恢复前自动保存本地副本。WebDAV 配置保持本机设置。");
                ui.label("不会自动修改桌面图标。跨电脑的快捷方式路径可能需要重新选择或创建。");
                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("确认恢复到本机").clicked() { self.start_webdav(ctx, DavAction::Restore); }
                        if ui.button("取消恢复").clicked() { self.webdav_download = None; }
                    });
                });
            }
        });
    }

    /// Append one line to the action log (HH:MM:SS, capped at 200).
    fn log_push(&mut self, msg: &str) {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
        self.log.push(format!("[{h:02}:{m:02}:{s:02}] {msg}"));
        if self.log.len() > 200 {
            let excess = self.log.len() - 200;
            self.log.drain(..excess);
        }
    }

    fn new(cc: &eframe::CreationContext) -> Self {
        // CJK font: egui's bundled fonts have no Chinese glyphs (tofu).
        // Prefer Microsoft YaHei (unpacked from msyh.ttc face #0 into a
        // standalone TTF for ab_glyph), falling back to DengXian (Deng.ttf)
        // and then SimHei (simhei.ttf).
        let mut fonts = egui::FontDefinitions::default();
        if let Some(bytes) = load_cjk_font_bytes() {
            fonts
                .font_data
                .insert("CJK".to_owned(), egui::FontData::from_owned(bytes).into());
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .insert(0, "CJK".to_owned());
            }
        }
        let mut heading_fonts = fonts
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default();
        if let Some(bytes) = installed_ui_font("MILanPro-Medium.ttf") {
            fonts
                .font_data
                .insert("UiMedium".into(), egui::FontData::from_owned(bytes).into());
            heading_fonts.insert(0, "UiMedium".into());
        }
        fonts
            .families
            .insert(egui::FontFamily::Name("UiHeading".into()), heading_fonts);
        cc.egui_ctx.set_fonts(fonts);
        cc.egui_ctx.all_styles_mut(|style| {
            style.text_styles.insert(
                egui::TextStyle::Heading,
                egui::FontId::new(19.0, egui::FontFamily::Name("UiHeading".into())),
            );
            style.spacing.item_spacing = egui::vec2(8.0, 8.0);
            style.spacing.button_padding = egui::vec2(10.0, 7.0);
        });
        let mut cfg = AppConfig::load();
        if !cfg.compact_layout {
            cfg.ui_scale = 1.0;
            cfg.compact_layout = true;
            let _ = cfg.save();
        }
        cc.egui_ctx.set_visuals(theme_visuals(cfg.dark_mode));
        let ui_scale = if cfg.ui_scale >= UI_SCALE_MIN && cfg.ui_scale <= UI_SCALE_MAX {
            cfg.ui_scale
        } else {
            UI_SCALE_DEFAULT
        };
        cc.egui_ctx.set_zoom_factor(ui_scale);
        let (webdav_settings, webdav_status) = match crate::webdav::Settings::load() {
            Ok(settings) => (settings, "填写地址和账号后，先测试连接".into()),
            Err(e) => (
                crate::webdav::Settings::default(),
                format!("读取配置失败：{e:#}"),
            ),
        };
        let mut app = App {
            folders: {
                let mut folders = lnk::known_folders();
                folders.insert(0, ("桌面（全部）".into(), PathBuf::new()));
                folders
            },
            folder_idx: 0,
            shortcuts: Vec::new(),
            icons: Vec::new(),
            filter: String::new(),
            selected: None,
            before: None,
            after: None,
            branch: String::new(),
            base: "white".to_string(),
            gradient_draft: Default::default(),
            asset_rx: None,
            asset_writing: false,
            resource_picker: None,
            theme_name: "我的主题".into(),
            theme_pending: None,
            theme_import_styles: true,
            custom_base: crate::gradient::Rgb::new(59, 130, 246), // #3B82F6
            glyph_scale: crate::composite::DEFAULT_GLYPH_SCALE,
            icon_radius: crate::composite::DEFAULT_ICON_RADIUS,
            base_size: crate::composite::DEFAULT_BASE_SIZE,
            base_radius: crate::composite::exponent_to_radius_pct(
                crate::composite::DEFAULT_BASE_EXPONENT,
            ),
            // Launch on the Current (fallback) tab: no network search
            // until the user explicitly enters the iTunes tab.
            tab: SourceTab::Current,
            search_query: String::new(),
            searched_for: None,
            candidates: Vec::new(),
            cand_selected: None,
            cand_textures: HashMap::new(),
            artwork_rx: None,
            search_rx: None,
            preview_rx: None,
            preview_ticket: Default::default(),
            apply_rx: None,
            recovery_rx: None,
            recovery_label: String::new(),
            recovery_progress: None,
            file_path: String::new(),
            file_design: false,
            studio_file: String::new(),
            studio_design: true,
            edit_history: History::new(EditState::default()),
            history_scope: None,
            studio_session: StudioSession::default(),
            session_dirty: None,
            session_read_error: false,
            status_seen: String::new(),
            status_since: std::time::Instant::now(),
            preview_master: None,
            export_rx: None,
            pending_image: None,
            style_presets: cfg.style_presets.clone(),
            draft_style: None,
            preset_error: String::new(),
            delete_preset: None,
            pack_plan: None,
            pack_selected: None,
            pack_scan_rx: None,
            pack_preview_rx: None,
            pack_before: None,
            pack_after: None,
            originals: HashMap::new(),
            applied: pipeline::applied_cache_load(),
            excluded: cfg
                .excluded_shortcuts
                .iter()
                .map(|p| lnk::path_key(p))
                .collect(),
            status: "就绪".to_string(),
            active_page: 0,
            editing: false,
            show_log: false,
            log: Vec::new(),
            webdav_settings,
            webdav_status,
            webdav_rx: None,
            webdav_mutating: false,
            webdav_download: None,
            snapshots: Vec::new(),
            small_preview_textures: Vec::new(),
            target_checks: HashMap::new(),
            target_check_rx: None,
            only_invalid: false,
            backup_inventory: None,
            backup_rx: None,
            backup_deleting: false,
            backup_selected: HashSet::new(),
            backup_confirm: None,
            selected_snaps: HashSet::new(),
            needs_admin: false,
            is_admin: lnk::is_elevated(),
            folder_rx: None,
            admin_rx: None,
            ui_scale,
            dark_mode: cfg.dark_mode,
            show_checkerboard: !cfg.hide_checkerboard,
        };
        app.reload_shortcuts(cc.egui_ctx.clone());
        app.load_studio_session(&cc.egui_ctx);
        if let Err(error) = pipeline::applied_cache_load_checked() {
            app.status = format!("恢复记录读取失败：{error:#}");
            let message = app.status.clone();
            app.log_push(&message);
        }
        app
    }

    fn reload_shortcuts(&mut self, ctx: egui::Context) {
        self.target_check_rx = None;
        self.target_checks.clear();
        self.only_invalid = false;
        self.invalidate_reads();
        self.shortcuts.clear();
        self.icons.clear();
        self.selected = None;
        self.editing = false;
        self.before = None;
        self.after = None;
        let Some((folder_name, dir)) = self.folders.get(self.folder_idx) else {
            return;
        };
        self.shortcuts = if dir.as_os_str().is_empty() {
            lnk::desktop_shortcuts()
        } else if folder_name.starts_with("开始菜单") {
            lnk::enumerate_recursive(dir)
        } else {
            lnk::enumerate(dir)
        };
        for s in &self.shortcuts {
            let tex = load_shortcut_icon(ctx.clone(), s);
            self.icons.push(tex);
        }
        self.originals = pipeline::oldest_originals();
        // Protected folders (Public Desktop/Start Menu/ProgramData) need
        // elevation to rewrite their .lnk files: surface it up front
        // instead of letting the first apply fail silently.
        self.needs_admin = !self.is_admin && lnk::is_protected_folder(dir);
        self.status = if self.needs_admin {
            format!(
                "已加载 {} 个快捷方式（此文件夹受系统保护，修改图标需管理员权限）",
                self.shortcuts.len()
            )
        } else {
            format!("已加载 {} 个快捷方式", self.shortcuts.len())
        };
        self.log_push(&format!(
            "加载文件夹 {}：{} 个快捷方式",
            if dir.as_os_str().is_empty() {
                folder_name.to_string()
            } else {
                dir.display().to_string()
            },
            self.shortcuts.len()
        ));
    }

    fn selected_shortcut(&self) -> Option<&ShortcutInfo> {
        self.selected.and_then(|i| self.shortcuts.get(i))
    }

    fn select(&mut self, ctx: &egui::Context, idx: usize) {
        self.invalidate_reads();
        self.after = None;
        self.selected = Some(idx);
        self.file_design = false;
        self.needs_admin = !self.is_admin
            && self
                .shortcuts
                .get(idx)
                .is_some_and(|s| lnk::is_protected_folder(std::path::Path::new(&s.path)));
        self.editing = true;
        self.candidates.clear();
        self.cand_selected = None;
        if let Some(s) = self.shortcuts.get(idx) {
            self.search_query = s.name.clone();
            self.before = self.icons.get(idx).cloned().flatten();
            // Restore this icon's zoom + three LinkEcho-basis sliders
            // from the applied cache; fresh icons start at defaults.
            // The base choice is restored only when this icon has a
            // cache entry: a fresh icon keeps the session's current
            // base (batch-applying with one base must not reset per
            // click). Unknown strings (e.g. legacy "blue") -> white.
            if let Some(e) = self.applied.get(&s.path) {
                self.base = match e.base.as_str() {
                    "pure" | "pick" => e.base.clone(),
                    s if crate::gradient::parse_linear(s).is_some() => e.base.clone(),
                    s => match crate::gradient::parse_custom(s) {
                        Some(c) => {
                            self.custom_base = c;
                            e.base.clone()
                        }
                        None => "white".to_string(),
                    },
                };
            }
            let scale = self
                .applied
                .get(&s.path)
                .map(|e| e.glyph_scale)
                .unwrap_or(crate::composite::DEFAULT_GLYPH_SCALE);
            self.glyph_scale = if (0.5..=1.2).contains(&scale) {
                scale
            } else {
                crate::composite::DEFAULT_GLYPH_SCALE
            };
            let entry = self.applied.get(&s.path);
            self.icon_radius = entry
                .map(|e| e.icon_radius)
                .filter(|v| (0.0..=0.5).contains(v))
                .unwrap_or(crate::composite::DEFAULT_ICON_RADIUS);
            self.base_size = entry
                .map(|e| e.base_size)
                .filter(|v| (0.6..=1.0).contains(v))
                .unwrap_or(crate::composite::DEFAULT_BASE_SIZE);
            self.base_radius = entry
                .map(|e| crate::composite::exponent_to_radius_pct(e.base_exponent))
                .filter(|v| (0.0..=100.0).contains(v))
                .unwrap_or(crate::composite::exponent_to_radius_pct(
                    crate::composite::DEFAULT_BASE_EXPONENT,
                ));
        }
        if let Some(path) = self.pending_image.take() {
            self.file_path = path.to_string_lossy().into_owned();
            self.tab = SourceTab::File;
            self.run_preview(ctx);
            return;
        }
        // Keep the iTunes tab across selections so batch processing does
        // not force a re-click per icon; other tabs reset to Current
        // (File would otherwise preview the previous icon's file).
        if self.tab != SourceTab::Itunes {
            self.tab = SourceTab::Current;
            self.run_preview(ctx);
        } else {
            self.maybe_auto_search(ctx);
        }
    }

    fn current_source(&self) -> Option<SourceSpec> {
        if self.active_page == 7 {
            return (!self.studio_file.is_empty()).then(|| SourceSpec {
                kind: if self.studio_design {
                    "file_design"
                } else {
                    "file"
                }
                .into(),
                path: self.studio_file.clone(),
                artwork_url: String::new(),
                bundle_id: String::new(),
            });
        }
        match self.tab {
            SourceTab::Current => Some(SourceSpec {
                kind: "current".into(),
                artwork_url: String::new(),
                bundle_id: String::new(),
                path: String::new(),
            }),
            SourceTab::File => {
                if self.file_path.is_empty() {
                    None
                } else {
                    Some(SourceSpec {
                        kind: if self.file_design {
                            "file_design"
                        } else {
                            "file"
                        }
                        .into(),
                        artwork_url: String::new(),
                        bundle_id: String::new(),
                        path: self.file_path.clone(),
                    })
                }
            }
            SourceTab::Itunes => {
                let (_, cand) = self.candidates.get(self.cand_selected?)?;
                Some(SourceSpec {
                    kind: "itunes".into(),
                    artwork_url: cand.artwork_url_1024(),
                    bundle_id: cand.bundle_id.clone(),
                    path: String::new(),
                })
            }
        }
    }

    fn run_preview(&mut self, ctx: &egui::Context) {
        self.preview_rx = None;
        self.preview_ticket
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.after = None;
        self.preview_master = None;
        self.small_preview_textures.clear();
        let shortcut_path = if self.active_page == 7 {
            String::new()
        } else {
            let Some(sc) = self.selected_shortcut() else {
                return;
            };
            sc.path.clone()
        };
        let Some(src) = self.current_source() else {
            self.status = "先选一个来源".to_string();
            return;
        };
        self.preview_rx = None;
        self.after = None;
        self.branch.clear();
        let ticket = self
            .preview_ticket
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let latest = self.preview_ticket.clone();
        let base = self.base.clone();
        let (scale, radius, size, exponent) = (
            self.glyph_scale,
            self.icon_radius,
            self.base_size,
            crate::composite::radius_pct_to_exponent(self.base_radius),
        );
        let (tx, rx) = std::sync::mpsc::channel();
        self.preview_rx = Some(rx);
        self.status = "正在生成预览…".into();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            // Debounce slider drags and discard superseded work before expensive I/O.
            std::thread::sleep(std::time::Duration::from_millis(120));
            if latest.load(std::sync::atomic::Ordering::Relaxed) != ticket {
                return;
            }
            let result = pipeline::compose_preview(
                &src,
                &shortcut_path,
                &base,
                false,
                scale,
                radius,
                size,
                exponent,
            )
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    fn finish_preview(&mut self, ctx: &egui::Context, result: PreviewReply) {
        match result {
            Ok((img, branch)) => {
                self.branch = branch;
                self.small_preview_textures = [16, 32, 48]
                    .into_iter()
                    .map(|size| {
                        let small = image::imageops::resize(
                            &img,
                            size,
                            size,
                            image::imageops::FilterType::CatmullRom,
                        );
                        (
                            size,
                            texture_from_image(ctx.clone(), &format!("preview-{size}"), &small),
                        )
                    })
                    .collect();
                self.preview_master = Some(img.clone());
                // Crisp preview: downscale large masters (1024px iTunes
                // artwork) with Lanczos3 instead of letting the GPU linearly
                // minify a 1024px texture to 192px (blurry/aliased).
                let (w, h) = img.dimensions();
                let img = if w.max(h) > 512 {
                    let scale = 512.0 / w.max(h) as f32;
                    let nw = (w as f32 * scale).round() as u32;
                    let nh = (h as f32 * scale).round() as u32;
                    image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3)
                } else {
                    img
                };
                self.after = Some(texture_from_image(ctx.clone(), "after", &img));
                self.status = "预览就绪".into();
            }
            Err(e) => {
                self.after = None;
                self.status = format!("预览失败: {e:#}");
            }
        }
    }

    /// LinkEcho-basis Changed rule for one row, fed by the recorded
    /// original when a snapshot exists for the shortcut.
    fn is_changed(&self, s: &ShortcutInfo) -> bool {
        pipeline::is_changed(
            &s.icon_path,
            &s.target,
            &s.kind,
            self.originals.get(&s.path).map(|x| x.as_str()),
        )
    }

    /// Refresh one row's in-memory icon path + left-list texture after
    /// an apply/restore changed what the .lnk points at.
    fn refresh_row_icon(&mut self, ctx: &egui::Context, lnk_path: &str, icon_path: &str) {
        if let Some(i) = self.shortcuts.iter().position(|s| s.path == lnk_path) {
            self.shortcuts[i].icon_path = icon_path.to_string();
            self.shortcuts[i].icon_index = lnk::parse(std::path::Path::new(lnk_path))
                .map(|info| info.icon_index)
                .unwrap_or(0);
            self.icons[i] = load_shortcut_icon(ctx.clone(), &self.shortcuts[i]);
        }
    }

    fn apply(&mut self, ctx: &egui::Context, items: Vec<ApplyItem>) {
        if items.is_empty() {
            self.status = "没有可应用的项".to_string();
            return;
        }
        if self.mutation_busy() {
            return;
        }
        self.invalidate_reads();
        let (tx, rx) = std::sync::mpsc::channel();
        self.apply_rx = Some(rx);
        self.status = format!("正在备份并应用 {} 个图标…", items.len());
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = pipeline::apply_items(&items, false, false);
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    fn finish_apply(
        &mut self,
        ctx: &egui::Context,
        results: Vec<ApplyResult>,
        snapshot_id: Option<String>,
    ) {
        let mut ok = 0;
        let mut fail = 0;
        let mut admin_needed = false;
        let mut errors = Vec::new();
        for r in results {
            if r.ok {
                ok += 1;
                if let Some(ico) = r.icon_path.clone() {
                    self.refresh_row_icon(ctx, &r.lnk_path, &ico);
                }
            } else {
                fail += 1;
                if r.needs_admin {
                    admin_needed = true;
                }
                let error = format!(
                    "{}：{}",
                    display_lnk_name(&r.lnk_path),
                    r.error.unwrap_or_default()
                );
                self.log_push(&error);
                errors.push(error);
            }
        }
        if ok > 0 || snapshot_id.is_some() {
            // A first-time apply created a snapshot: pick up its
            // original so the 撤回 button appears for the item.
            self.originals = pipeline::oldest_originals();
            // And a cache entry: the 重新应用 button appears when the
            // icon later reverts (e.g. after an app update).
            self.applied = pipeline::applied_cache_load();
        }
        if admin_needed {
            self.needs_admin = true;
        }
        self.status = format!(
            "应用完成：成功 {ok}，失败 {fail}{}",
            snapshot_id
                .map(|id| format!(" · 快照 {id}"))
                .unwrap_or_default()
        );
        if let Some(error) = errors.first() {
            self.status.push_str(&format!("；{error}（详情见日志）"));
        }
        self.log_push(&format!("应用：成功 {ok}，失败 {fail}"));
    }

    fn prepare_icon_pack(&mut self, ctx: &egui::Context, dir: &std::path::Path) {
        if self.mutation_busy() {
            return;
        }
        self.invalidate_reads();
        self.pack_plan = None;
        self.pack_selected = None;
        let shortcuts = self.shortcuts.clone();
        let excluded = self.excluded.clone();
        let dir = dir.to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        self.pack_scan_rx = Some(rx);
        self.status = "正在读取图标包并准备匹配预览…".into();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result =
                crate::icon_pack::plan(shortcuts, excluded, dir).map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    fn apply_selected(&mut self, ctx: &egui::Context) {
        let Some(sc) = self.selected_shortcut().cloned() else {
            return;
        };
        // No UWP gate here (LinkEcho-basis): single-apply works on
        // UWP .lnk — SetIconLocation on a desktop shortcut is honored
        // by Explorer. Only restore-to-target is skipped for UWP
        // (no target to point back at); UWP restore = delete the
        // shortcut + re-create it via 创建应用快捷方式.
        let Some(src) = self.current_source() else {
            self.status = "先选一个来源".to_string();
            return;
        };
        self.apply(
            ctx,
            vec![ApplyItem {
                lnk_path: sc.path,
                source: src,
                base: self.base.clone(),
                glyph_scale: self.glyph_scale,
                icon_radius: self.icon_radius,
                base_size: self.base_size,
                base_exponent: crate::composite::radius_pct_to_exponent(self.base_radius),
            }],
        );
    }

    /// Shared bookkeeping after any restore path (single undo, batch,
    /// restore-all): point the rows back at the restored icon (the
    /// LinkEcho-basis badge follows from it), refresh the left-list
    /// icon textures, surface admin need, set the status.
    fn after_restore(&mut self, ctx: &egui::Context, results: Vec<ApplyResult>, what: &str) {
        let mut ok = 0;
        let mut fail = 0;
        let mut admin_needed = false;
        let mut restored: Vec<String> = Vec::new();
        let mut failures = Vec::new();
        for r in &results {
            if r.ok {
                ok += 1;
                self.log_push(&format!("{what}成功：{}", display_lnk_name(&r.lnk_path)));
                restored.push(r.lnk_path.clone());
                if let Some(ico) = &r.icon_path {
                    self.refresh_row_icon(ctx, &r.lnk_path, ico);
                }
            } else {
                fail += 1;
                failures.push(format!(
                    "{}：{}",
                    display_lnk_name(&r.lnk_path),
                    r.error.as_deref().unwrap_or("未知错误")
                ));
                if r.needs_admin {
                    admin_needed = true;
                }
            }
        }
        if let Some(sc) = self.selected_shortcut().cloned() {
            if restored.contains(&sc.path) {
                if let Some(idx) = self.shortcuts.iter().position(|s| s.path == sc.path) {
                    self.before = self.icons.get(idx).cloned().flatten();
                }
            }
        }
        if admin_needed {
            self.needs_admin = true;
        }
        let admin_hint = if admin_needed {
            "（部分需要管理员权限）"
        } else {
            ""
        };
        self.status = if fail == 0 {
            format!("{what}完成：已还原 {ok} 个{admin_hint}")
        } else if ok == 0 {
            format!("{what}失败：{fail} 个未还原{admin_hint}")
        } else {
            format!("{what}完成：成功 {ok}，失败 {fail}{admin_hint}")
        };
        for failure in &failures {
            self.log_push(failure);
        }
        if let Some(first) = failures.first() {
            self.status.push_str(&format!("；{first}（详情见日志）"));
        }
        self.log_push(&format!("{what}：成功 {ok}，失败 {fail}"));
    }

    /// 一键恢复：把 applied.json 缓存里的全部已应用图标一次性
    /// 恢复（应用更新把图标打回原形 / 重装系统后，一键全换回来）。
    /// 逐条复用 pipeline::reapply（每条先记快照、可撤回）；已是缓存
    /// 图标的跳过不计入成功；快捷方式已不存在 / 缓存 ICO 丢失的计
    /// 入失败并报原因。跨文件夹条目直接解析 .lnk 判定（不限于当前
    /// 文件夹列表），所以跳过计数对所有条目都准确。
    fn restore_all_from_cache(&mut self, ctx: &egui::Context) {
        self.start_recovery(
            ctx,
            crate::recovery::Action::Cache(self.excluded.clone()),
            "一键恢复",
        );
    }

    /// Auto-search when entering the iTunes tab (or selecting a new item
    /// while on it), unless this exact (shortcut, query) was already
    /// searched and still has candidates. The manual 搜索 button still
    /// forces a search via search().
    fn maybe_auto_search(&mut self, ctx: &egui::Context) {
        let Some(sc) = self.selected_shortcut().cloned() else {
            return;
        };
        let q = self.search_query.trim().to_string();
        if q.is_empty() {
            return;
        }
        let key = format!("{}\n{q}", sc.path);
        if self.searched_for.as_deref() == Some(key.as_str()) && !self.candidates.is_empty() {
            if self.cand_selected.is_some() {
                self.run_preview(ctx);
            }
            return;
        }
        self.search(ctx);
    }

    fn search(&mut self, ctx: &egui::Context) {
        let q = self.search_query.trim().to_string();
        if q.is_empty() {
            return;
        }
        let Some(sc) = self.selected_shortcut() else {
            return;
        };
        let key = format!("{}\n{q}", sc.path);
        self.preview_rx = None;
        self.preview_ticket
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.after = None;
        self.candidates.clear();
        self.cand_selected = None;
        self.cand_textures.clear();
        self.artwork_rx = None;
        self.searched_for = None;
        self.status = "搜索中…".into();
        let (tx, rx) = std::sync::mpsc::channel();
        self.search_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = itunes::search_all(&q).map_err(|e| format!("{e:#}"));
            let _ = tx.send((key, result));
            ctx.request_repaint();
        });
    }

    fn finish_search(
        &mut self,
        ctx: &egui::Context,
        key: String,
        result: Result<Vec<(u32, AppCandidate)>, String>,
    ) {
        let current = self
            .selected_shortcut()
            .map(|sc| format!("{}\n{}", sc.path, self.search_query.trim()));
        if self.tab != SourceTab::Itunes || current.as_deref() != Some(key.as_str()) {
            return;
        }
        match result {
            Ok(cands) => {
                self.searched_for = Some(key);
                self.candidates = cands;
                // Thumbnails: fetch artwork on a background thread so
                // the UI never blocks on thumbnail downloads.
                self.cand_textures.clear();
                let mut seen = HashSet::new();
                let urls: Vec<String> = self
                    .candidates
                    .iter()
                    .map(|(_, c)| pipeline::thumb_url(&c.artwork_url_512))
                    .filter(|t| !t.is_empty() && seen.insert(t.clone()))
                    .collect();
                if !urls.is_empty() {
                    let (tx, rx) = std::sync::mpsc::channel::<(String, Vec<u8>)>();
                    self.artwork_rx = Some(rx);
                    let ctx = ctx.clone();
                    std::thread::spawn(move || {
                        let Ok(client) = reqwest::blocking::Client::builder()
                            .timeout(std::time::Duration::from_secs(15))
                            .build()
                        else {
                            return;
                        };
                        for u in urls {
                            if let Ok(resp) = client
                                .get(u.as_str())
                                .send()
                                .and_then(|r| r.error_for_status())
                            {
                                if let Ok(bytes) = resp.bytes() {
                                    if tx.send((u, bytes.to_vec())).is_err() {
                                        break;
                                    }
                                    ctx.request_repaint();
                                }
                            }
                        }
                    });
                }
                if self.candidates.is_empty() {
                    self.cand_selected = None;
                    self.status = "双区均无结果，建议用“当前图标兜底”或本地文件".to_string();
                } else {
                    // Auto-preview the first candidate: otherwise the right
                    // side stays stale/blank after a search until the user
                    // manually clicks a candidate.
                    self.cand_selected = Some(0);
                    let n = self.candidates.len();
                    self.run_preview(ctx);
                    self.status = format!("找到 {n} 个候选（已自动预览第 1 个）");
                }
            }
            Err(e) => {
                self.status = format!("搜索失败: {e:#}");
            }
        }
    }
}

fn texture_from_image(ctx: egui::Context, name: &str, img: &image::RgbaImage) -> TextureHandle {
    let (w, h) = img.dimensions();
    let pixels: Vec<u8> = img.clone().into_raw();
    let color = ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &pixels);
    ctx.load_texture(name, color, TextureOptions::default())
}

/// Extract face #0 from a TrueType Collection (.ttc) into a standalone
/// TrueType (.ttf) buffer so `ab_glyph` can parse `msyh.ttc` directly.
fn ttc_first_ttf(ttc: &[u8]) -> Option<Vec<u8>> {
    if ttc.len() < 16 || &ttc[0..4] != b"ttcf" {
        return None;
    }
    let num_fonts = u32::from_be_bytes(ttc[8..12].try_into().ok()?) as usize;
    if num_fonts == 0 {
        return None;
    }
    let dir_off = u32::from_be_bytes(ttc[12..16].try_into().ok()?) as usize;
    if dir_off.checked_add(12)? > ttc.len() {
        return None;
    }
    let num_tables = u16::from_be_bytes(ttc[dir_off + 4..dir_off + 6].try_into().ok()?) as usize;
    let header_len = 12usize.checked_add(num_tables.checked_mul(16)?)?;
    if dir_off.checked_add(header_len)? > ttc.len() {
        return None;
    }
    let mut out = Vec::with_capacity(ttc.len() / 2);
    out.extend_from_slice(&ttc[dir_off..dir_off + header_len]);
    for i in 0..num_tables {
        let rec_src = dir_off + 12 + i * 16;
        let orig_off = u32::from_be_bytes(ttc[rec_src + 8..rec_src + 12].try_into().ok()?) as usize;
        let length = u32::from_be_bytes(ttc[rec_src + 12..rec_src + 16].try_into().ok()?) as usize;
        if orig_off.checked_add(length)? > ttc.len() {
            return None;
        }
        let new_off = out.len() as u32;
        let rec_dst = 12 + i * 16;
        out[rec_dst + 8..rec_dst + 12].copy_from_slice(&new_off.to_be_bytes());
        out.extend_from_slice(&ttc[orig_off..orig_off + length]);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    Some(out)
}

/// Prefer the user's installed MiLan Pro, then static MiSans and system CJK fonts.
fn installed_ui_font(file: &str) -> Option<Vec<u8>> {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .map(|root| root.join("Microsoft/Windows/Fonts").join(file));
    if let Some(path) = local {
        if let Ok(bytes) = std::fs::read(path) {
            return Some(bytes);
        }
    }
    let windows = std::env::var_os("WINDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
    std::fs::read(windows.join("Fonts").join(file)).ok()
}
fn load_cjk_font_bytes() -> Option<Vec<u8>> {
    for file in ["MILanPro-Normal.ttf", "MiSans-Normal.ttf"] {
        if let Some(bytes) = installed_ui_font(file) {
            return Some(bytes);
        }
    }
    if let Some(ttc) = installed_ui_font("msyh.ttc") {
        if let Some(ttf) = ttc_first_ttf(&ttc) {
            return Some(ttf);
        }
    }
    installed_ui_font("Deng.ttf").or_else(|| installed_ui_font("simhei.ttf"))
}

fn load_shortcut_icon(ctx: egui::Context, s: &ShortcutInfo) -> Option<TextureHandle> {
    let (file, idx) = if !s.icon_path.is_empty() {
        (s.icon_path.clone(), s.icon_index)
    } else {
        (s.target.clone(), 0)
    };
    let png = lnk::extract_icon_png(&file, idx, 256).unwrap_or_else(pipeline::placeholder_png);
    let img = image::load_from_memory(&png).ok()?.to_rgba8();
    Some(texture_from_image(ctx, &format!("icon-{}", s.path), &img))
}

// ---------- FlClash-style theme + widgets ----------

/// Accent (Claude terracotta), shared by both themes.
const ACCENT: egui::Color32 = egui::Color32::from_rgb(0xC9, 0x64, 0x42);

/// UI zoom limits / step (Ctrl+wheel steps by 0.05, the sidebar buttons by 0.1).
const UI_SCALE_MIN: f32 = 0.7;
const UI_SCALE_MAX: f32 = 1.6;
const UI_SCALE_DEFAULT: f32 = 1.0;

/// Theme-dependent palette: the base egui visuals cover panels/widgets;
/// these cover the custom-drawn cards, nav, segmented controls, badges.
struct Palette {
    card: egui::Color32,
    card_border: egui::Color32,
    seg_bg: egui::Color32,
    nav_sel_bg: egui::Color32,
    nav_sel_fg: egui::Color32,
    text: egui::Color32,
    secondary: egui::Color32,
    weak: egui::Color32,
    outline_bg: egui::Color32,
    sel_fill: egui::Color32,
    bg: egui::Color32,
}

impl Palette {
    fn of(dark: bool) -> Self {
        if dark {
            Self {
                card: egui::Color32::from_rgb(0x22, 0x25, 0x2B),
                card_border: egui::Color32::from_rgb(0x4A, 0x49, 0x43),
                seg_bg: egui::Color32::from_rgb(0x2B, 0x2E, 0x35),
                nav_sel_bg: egui::Color32::from_rgb(0x4A, 0x31, 0x28),
                nav_sel_fg: egui::Color32::from_rgb(0xF0, 0xA4, 0x84),
                text: egui::Color32::from_rgb(0xF6, 0xF5, 0xF2),
                secondary: egui::Color32::from_rgb(0xA5, 0xA8, 0xB2),
                weak: egui::Color32::from_rgb(0x8F, 0x8B, 0x7A),
                outline_bg: egui::Color32::from_rgb(0x2B, 0x2E, 0x35),
                sel_fill: egui::Color32::from_rgb(0x3F, 0x2D, 0x25),
                bg: egui::Color32::from_rgb(0x17, 0x19, 0x1D),
            }
        } else {
            Self {
                card: egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
                card_border: egui::Color32::from_rgb(0xE7, 0xE6, 0xE2),
                seg_bg: egui::Color32::from_rgb(0xEF, 0xEE, 0xEA),
                nav_sel_bg: egui::Color32::from_rgb(0xF0, 0xDD, 0xD2),
                nav_sel_fg: egui::Color32::from_rgb(0xB5, 0x53, 0x2F),
                text: egui::Color32::from_rgb(0x24, 0x26, 0x2B),
                secondary: egui::Color32::from_rgb(0x71, 0x74, 0x7C),
                weak: egui::Color32::from_rgb(0x8F, 0x8B, 0x7A),
                outline_bg: egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
                sel_fill: egui::Color32::from_rgb(0xFB, 0xEF, 0xE8),
                bg: egui::Color32::from_rgb(0xF6, 0xF5, 0xF2),
            }
        }
    }
}

fn theme_visuals(dark: bool) -> egui::Visuals {
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    if dark {
        v.panel_fill = egui::Color32::from_rgb(0x17, 0x19, 0x1D);
        v.window_fill = egui::Color32::from_rgb(0x22, 0x25, 0x2B);
        v.extreme_bg_color = egui::Color32::from_rgb(0x17, 0x19, 0x1D);
        v.faint_bg_color = egui::Color32::from_rgb(0x2B, 0x2E, 0x35);
        v.widgets.inactive.bg_fill = egui::Color32::from_rgb(0x3E, 0x3D, 0x38);
        v.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(0x2B, 0x2E, 0x35);
        v.widgets.inactive.bg_stroke =
            egui::Stroke::new(1.0, egui::Color32::from_rgb(0x56, 0x54, 0x4C));
        v.widgets.hovered.bg_fill = egui::Color32::from_rgb(0x4C, 0x4A, 0x43);
        v.widgets.hovered.bg_stroke = egui::Stroke::new(1.5, ACCENT);
        v.widgets.active.bg_fill = ACCENT;
        v.widgets.active.bg_stroke = egui::Stroke::new(1.5, ACCENT);
        v.widgets.noninteractive.fg_stroke.color = egui::Color32::from_rgb(0xF6, 0xF5, 0xF2);
        v.widgets.inactive.fg_stroke.color = egui::Color32::from_rgb(0xF6, 0xF5, 0xF2);
    } else {
        v.panel_fill = egui::Color32::from_rgb(0xF6, 0xF5, 0xF2);
        v.window_fill = egui::Color32::from_rgb(0xFF, 0xFF, 0xFF);
        v.extreme_bg_color = egui::Color32::from_rgb(0xFF, 0xFF, 0xFF);
        v.faint_bg_color = egui::Color32::from_rgb(0xEF, 0xEE, 0xEA);
        v.widgets.inactive.bg_fill = egui::Color32::from_rgb(0xE2, 0xDE, 0xD0);
        v.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(0xE6, 0xE3, 0xD6);
        v.widgets.inactive.bg_stroke =
            egui::Stroke::new(1.0, egui::Color32::from_rgb(0xCC, 0xC7, 0xB8));
        v.widgets.hovered.bg_fill = egui::Color32::from_rgb(0xF0, 0xDD, 0xD2);
        v.widgets.hovered.bg_stroke = egui::Stroke::new(1.5, ACCENT);
        v.widgets.active.bg_fill = egui::Color32::from_rgb(0xE9, 0xC9, 0xB8);
        v.widgets.active.bg_stroke = egui::Stroke::new(1.5, ACCENT);
        v.widgets.noninteractive.fg_stroke.color = egui::Color32::from_rgb(0x24, 0x26, 0x2B);
        v.widgets.inactive.fg_stroke.color = egui::Color32::from_rgb(0x24, 0x26, 0x2B);
    }
    v.slider_trailing_fill = true;
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = egui::Stroke::new(1.5, egui::Color32::WHITE);
    v.hyperlink_color = ACCENT;
    for w in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
    ] {
        w.corner_radius = 999.0.into();
    }
    v.window_corner_radius = 20.0.into();
    v.menu_corner_radius = 16.0.into();
    v
}

/// A frame whose whole area is clickable (cards, nav items).
fn clickable_frame<R>(
    ui: &mut egui::Ui,
    frame: egui::Frame,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> (egui::Response, R) {
    let inner = frame.show(ui, add_contents);
    let resp = inner.response.interact(egui::Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    (resp, inner.inner)
}

/// Segmented control: pill container, selected segment white.
fn segmented(ui: &mut egui::Ui, pal: &Palette, options: &[&str], current: usize) -> Option<usize> {
    let n = options.len().max(1) as f32;
    let mut clicked = None;
    egui::Frame::new()
        .fill(pal.seg_bg)
        .corner_radius(999.0)
        .inner_margin(egui::Margin::same(3))
        .show(ui, |ui| {
            let total_w = ui.available_width();
            ui.set_max_width(total_w);
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.spacing_mut().button_padding = egui::Vec2::new(4.0, 4.0);
            ui.horizontal(|ui| {
                let seg_w = (total_w / n).floor();
                for (i, opt) in options.iter().enumerate() {
                    let sel = i == current;
                    let mut rt = egui::RichText::new(*opt).size(12.0).color(if sel {
                        pal.text
                    } else {
                        pal.secondary
                    });
                    if sel {
                        rt = rt.strong();
                    }
                    let btn = egui::Button::new(rt)
                        .fill(if sel {
                            pal.card
                        } else {
                            egui::Color32::TRANSPARENT
                        })
                        .stroke(if sel {
                            egui::Stroke::new(1.0, pal.card_border)
                        } else {
                            egui::Stroke::NONE
                        })
                        .corner_radius(999.0);
                    if ui.add_sized([seg_w, 28.0], btn).clicked() {
                        clicked = Some(i);
                    }
                }
            });
        });
    clicked
}

fn icon_button(
    ui: &mut egui::Ui,
    pal: &Palette,
    icon: Icon,
    label: &str,
    width: Option<f32>,
    primary: bool,
) -> egui::Response {
    let font = egui::FontId::proportional(13.0);
    let fg = if primary {
        egui::Color32::WHITE
    } else {
        pal.text
    };
    let galley = ui.painter().layout_no_wrap(label.into(), font, fg);
    let desired = width.unwrap_or(galley.size().x + 44.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(desired, 36.0), egui::Sense::click());
    let fill = if primary {
        ACCENT
    } else if response.hovered() {
        pal.seg_bg
    } else {
        egui::Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 12.0, fill);
    let offset = ((rect.width() - (galley.size().x + 23.0)) / 2.0).max(10.0);
    let opacity = if ui.is_enabled() { 1.0 } else { 0.45 };
    let icon_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left() + offset, rect.center().y - 8.0),
        egui::vec2(16.0, 16.0),
    );
    draw_icon(ui.painter(), icon_rect, icon, fg.gamma_multiply(opacity));
    ui.painter().galley(
        egui::pos2(
            icon_rect.right() + 7.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        fg.gamma_multiply(opacity),
    );
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), label));
    response
}
fn icon_menu_button<R>(
    ui: &mut egui::Ui,
    pal: &Palette,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<Option<R>> {
    let result = ui.menu_button("  ", add);
    draw_icon(
        ui.painter(),
        egui::Rect::from_center_size(result.response.rect.center(), egui::vec2(17.0, 17.0)),
        Icon::Ellipsis,
        pal.secondary,
    );
    result.response.clone().on_hover_text("更多操作");
    result
}
fn primary_button(ui: &mut egui::Ui, pal: &Palette, label: &str, width: f32) -> egui::Response {
    icon_button(ui, pal, Icon::Check, label, Some(width), true)
}
fn outline_button(ui: &mut egui::Ui, pal: &Palette, label: &str) -> egui::Response {
    let icon = match label {
        "撤回" | "取消排除" => Some(Icon::Undo2),
        "排除" => Some(Icon::ShieldCheck),
        "重新应用" | "还原到目标" => Some(Icon::RotateCw),
        "选择文件…" | "打开目标文件夹" => Some(Icon::FolderOpen),
        "属性" => Some(Icon::AppWindow),
        _ => None,
    };
    if let Some(icon) = icon {
        icon_button(ui, pal, icon, label, None, false)
    } else {
        ui.add(
            egui::Button::new(egui::RichText::new(label).color(pal.text))
                .fill(pal.outline_bg)
                .corner_radius(10.0),
        )
    }
}

/// Pill badge on a card: (bg, fg) per kind.
fn badge_style(dark: bool, kind: &str) -> (egui::Color32, egui::Color32) {
    match (dark, kind) {
        (_, "broken") => (
            if dark {
                egui::Color32::from_rgb(80, 35, 35)
            } else {
                egui::Color32::from_rgb(255, 227, 227)
            },
            if dark {
                egui::Color32::from_rgb(255, 165, 165)
            } else {
                egui::Color32::from_rgb(170, 48, 48)
            },
        ),
        (_, "changed") => {
            if dark {
                (
                    egui::Color32::from_rgb(0x4A, 0x31, 0x28),
                    egui::Color32::from_rgb(0xF0, 0xA4, 0x84),
                )
            } else {
                (
                    egui::Color32::from_rgb(0xF0, 0xDD, 0xD2),
                    egui::Color32::from_rgb(0xB5, 0x53, 0x2F),
                )
            }
        }
        (_, "cached") => {
            if dark {
                (
                    egui::Color32::from_rgb(0x1E, 0x4A, 0x2A),
                    egui::Color32::from_rgb(0x7E, 0xD4, 0x91),
                )
            } else {
                (
                    egui::Color32::from_rgb(0xDF, 0xF5, 0xE3),
                    egui::Color32::from_rgb(0x1E, 0x7B, 0x34),
                )
            }
        }
        (_, "excluded") => {
            if dark {
                (
                    egui::Color32::from_rgb(0x4A, 0x24, 0x24),
                    egui::Color32::from_rgb(0xFF, 0x9D, 0x8A),
                )
            } else {
                (
                    egui::Color32::from_rgb(0xFD, 0xE8, 0xE8),
                    egui::Color32::from_rgb(0xB0, 0x26, 0x00),
                )
            }
        }
        _ => {
            if dark {
                (
                    egui::Color32::from_rgb(0x2A, 0x2E, 0x34),
                    egui::Color32::from_rgb(0x9A, 0xA0, 0xA7),
                )
            } else {
                (
                    egui::Color32::from_rgb(0xEE, 0xF0, 0xF2),
                    egui::Color32::from_rgb(0x6A, 0x6E, 0x75),
                )
            }
        }
    }
}

fn avatar_color(name: &str) -> egui::Color32 {
    const PALETTE: [(u8, u8, u8); 8] = [
        (0x2F, 0x7B, 0xF6),
        (0x1D, 0xB9, 0x54),
        (0x10, 0xA3, 0x7F),
        (0x29, 0xA9, 0xEB),
        (0xF2, 0x6B, 0x1D),
        (0x7C, 0x5C, 0xFF),
        (0xE6, 0x21, 0x17),
        (0x0F, 0x7B, 0x6C),
    ];
    let mut h: u32 = 0;
    for b in name.bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as u32);
    }
    let (r, g, b) = PALETTE[(h as usize) % PALETTE.len()];
    egui::Color32::from_rgb(r, g, b)
}

/// `badge`: right-aligned count pill (green, e.g. the applied-cache
/// count on 一键恢复). None = no badge.
/// Display name for a .lnk path in status messages: file stem
/// ("Docker Desktop" from "...\Docker Desktop.lnk").
fn display_lnk_name(lnk_path: &str) -> String {
    std::path::Path::new(lnk_path)
        .file_stem()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| lnk_path.to_string())
}

/// LinkEcho-basis "查看属性": open the standard Windows Properties
/// dialog for a .lnk (ShellExecute "properties" verb).
#[cfg(windows)]
fn open_properties(lnk_path: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;
    let verb: Vec<u16> = "properties\0".encode_utf16().collect();
    let file: Vec<u16> = lnk_path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOW,
        );
    }
}

#[cfg(not(windows))]
fn open_properties(_lnk_path: &str) {}

/// LinkEcho-basis "打开目标文件夹": open Explorer with the shortcut's
/// target selected. No-op for UWP (no target).
#[cfg(windows)]
fn open_target_folder(target: &str) {
    let _ = std::process::Command::new("explorer")
        .arg(format!("/select,{}", target))
        .spawn();
}

#[cfg(not(windows))]
fn open_target_folder(_target: &str) {}

impl App {
    fn compact_editor(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, pal: &Palette) {
        let Some(sc) = self.selected_shortcut().cloned() else {
            self.editing = false;
            return;
        };
        let mut back = false;
        ui.horizontal(|ui| {
            back = icon_button(ui, pal, Icon::ArrowLeft, "返回", None, false)
                .on_hover_text("返回列表 · Esc")
                .clicked();
            let title_width = (ui.available_width() - 140.0).max(80.0);
            ui.allocate_ui_with_layout(
                egui::vec2(title_width, 28.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_min_width(title_width);
                    ui.set_max_width(title_width);
                    ui.add(
                        egui::Label::new(egui::RichText::new(&sc.name).strong().size(17.0))
                            .truncate(),
                    )
                    .on_hover_text(&sc.name);
                },
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                icon_menu_button(ui, &pal, |ui| {
                    ui.vertical(|ui| {
                        if sc.kind != "uwp"
                            && self.is_changed(&sc)
                            && outline_button(ui, &pal, "还原到目标").clicked()
                        {
                            self.start_recovery(
                                ctx,
                                crate::recovery::Action::Target(sc.path.clone()),
                                "还原到目标",
                            );
                        }
                        let excluded = self.excluded.contains(&lnk::path_key(&sc.path));
                        if outline_button(ui, &pal, if excluded { "取消排除" } else { "排除" })
                            .on_hover_text("图标包与一键恢复会跳过排除项")
                            .clicked()
                        {
                            self.toggle_exclusion(&sc.path);
                        }
                        if outline_button(ui, &pal, "属性").clicked() {
                            open_properties(&sc.path);
                        }
                        if sc.kind != "uwp"
                            && !sc.target.is_empty()
                            && outline_button(ui, &pal, "打开目标文件夹").clicked()
                        {
                            open_target_folder(&sc.target);
                        }
                    });

                    if sc.kind == "uwp" && ui.button("创建应用快捷方式").clicked() {
                        self.open_folder(ctx, crate::shell::Folder::Apps);
                        ui.close();
                    }
                });
                self.history_buttons(ui, ctx, pal, true);
            });
        });
        if back {
            self.editing = false;
            self.invalidate_reads();
            return;
        }
        ui.add_space(10.0);
        if let Some(health) = self.target_checks.get(&lnk::path_key(&sc.path)) {
            match health {
                crate::maintenance::TargetHealth::Missing => {
                    ui.colored_label(
                        egui::Color32::from_rgb(195, 72, 64),
                        "目标文件不存在，请检查应用是否已移动或卸载",
                    )
                    .on_hover_text(&sc.target);
                }
                crate::maintenance::TargetHealth::Unknown(reason) => {
                    ui.label(reason);
                }
                _ => {}
            }
        }
        let scroll_height = (ui.available_height() - 54.0).max(80.0);
        egui::ScrollArea::vertical()
            .id_salt(("compact_editor", &sc.path))
            .auto_shrink([false, false])
            .max_height(scroll_height)
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width());
                // Symmetrical before / after comparison stage.
                egui::Frame::new()
                    .fill(pal.seg_bg)
                    .corner_radius(14.0)
                    .inner_margin(egui::Margin::symmetric(10, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let pv = ((ui.available_width() - 36.0) / 2.0).clamp(64.0, 84.0);
                        ui.horizontal(|ui| {
                            let inset = ((ui.available_width() - (pv * 2.0 + 36.0)) / 2.0).max(0.0);
                            ui.add_space(inset);
                            ui.allocate_ui_with_layout(
                                egui::Vec2::new(pv, pv + 22.0),
                                egui::Layout::top_down(egui::Align::Center),
                                |ui| {
                                    if let Some(tex) = &self.before {
                                        checker_image(
                                            ui,
                                            tex,
                                            egui::Vec2::splat(pv),
                                            self.dark_mode,
                                            self.show_checkerboard,
                                        );
                                    } else {
                                        let (r, _) = ui.allocate_exact_size(
                                            egui::Vec2::splat(pv),
                                            egui::Sense::hover(),
                                        );
                                        ui.painter().rect_filled(r, 16.0, pal.card);
                                    }
                                    ui.add_space(4.0);
                                    ui.label(
                                        egui::RichText::new("改前").color(pal.secondary).size(11.5),
                                    );
                                },
                            );
                            let (arrow_rect, _) = ui.allocate_exact_size(
                                egui::Vec2::new(20.0, pv),
                                egui::Sense::hover(),
                            );
                            let ac = arrow_rect.center();
                            let a_stroke = egui::Stroke::new(1.8, pal.weak);
                            ui.painter().line_segment(
                                [
                                    egui::Pos2::new(ac.x - 6.0, ac.y),
                                    egui::Pos2::new(ac.x + 6.0, ac.y),
                                ],
                                a_stroke,
                            );
                            ui.painter().line_segment(
                                [
                                    egui::Pos2::new(ac.x + 2.0, ac.y - 4.0),
                                    egui::Pos2::new(ac.x + 6.0, ac.y),
                                ],
                                a_stroke,
                            );
                            ui.painter().line_segment(
                                [
                                    egui::Pos2::new(ac.x + 2.0, ac.y + 4.0),
                                    egui::Pos2::new(ac.x + 6.0, ac.y),
                                ],
                                a_stroke,
                            );
                            ui.allocate_ui_with_layout(
                                egui::Vec2::new(pv, pv + 22.0),
                                egui::Layout::top_down(egui::Align::Center),
                                |ui| {
                                    if let Some(tex) = &self.after {
                                        checker_image(
                                            ui,
                                            tex,
                                            egui::Vec2::splat(pv),
                                            self.dark_mode,
                                            self.show_checkerboard,
                                        );
                                    } else {
                                        let (r, _) = ui.allocate_exact_size(
                                            egui::Vec2::splat(pv),
                                            egui::Sense::hover(),
                                        );
                                        ui.painter().rect_filled(r, 16.0, pal.card);
                                        ui.painter().text(
                                            r.center(),
                                            egui::Align2::CENTER_CENTER,
                                            "待预览",
                                            egui::FontId::proportional(11.5),
                                            pal.weak,
                                        );
                                    }
                                    ui.add_space(4.0);
                                    ui.label(
                                        egui::RichText::new("改后预览")
                                            .color(pal.secondary)
                                            .size(11.5),
                                    );
                                },
                            );
                        });
                    });
                ui.add_space(10.0);
                self.small_previews(ui, pal);
                // Source segmented control (4-char symmetrical labels).
                let tab_idx = match self.tab {
                    SourceTab::Itunes => 0,
                    SourceTab::Current => 1,
                    SourceTab::File => 2,
                };
                if let Some(i) = segmented(ui, &pal, &["在线搜索", "当前图标", "本地文件"], tab_idx)
                {
                    self.preview_rx = None;
                    self.preview_ticket
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    match i {
                        0 => {
                            self.tab = SourceTab::Itunes;
                            // Entering the tab searches automatically: no extra
                            // click on 搜索 needed.
                            self.maybe_auto_search(&ctx);
                        }
                        1 => {
                            self.tab = SourceTab::Current;
                            self.run_preview(&ctx);
                        }
                        _ => {
                            self.tab = SourceTab::File;
                            if !self.file_path.is_empty() {
                                self.run_preview(&ctx);
                            } else {
                                self.after = None;
                                self.status = "选择本地文件后自动预览".to_string();
                            }
                        }
                    }
                }
                ui.add_space(8.0);
                match self.tab {
                    SourceTab::Itunes => {
                        ui.horizontal(|ui| {
                            // Bound the TextEdit so it leaves room for 搜索.
                            let edit_w = (ui.available_width() - 64.0).max(80.0);
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut self.search_query)
                                        .desired_width(edit_w),
                                )
                                .lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                            {
                                self.search(&ctx);
                            }
                            if outline_button(ui, &pal, "搜索").clicked() {
                                self.search(&ctx);
                            }
                        });
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .id_salt("cand_scroll")
                            .max_height(190.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.set_max_width(ui.available_width());
                                let cands = self.candidates.clone();
                                for (i, (score, c)) in cands.iter().enumerate() {
                                    let sel = self.cand_selected == Some(i);
                                    let initial =
                                        c.track_name.chars().next().unwrap_or('?').to_string();
                                    let tex = self
                                        .cand_textures
                                        .get(&pipeline::thumb_url(&c.artwork_url_512))
                                        .cloned();
                                    let (resp, _) = clickable_frame(
                                        ui,
                                        egui::Frame::new()
                                            .fill(if sel { pal.sel_fill } else { pal.card })
                                            .stroke(egui::Stroke::new(
                                                1.5,
                                                if sel { ACCENT } else { pal.card_border },
                                            ))
                                            .corner_radius(14.0)
                                            .inner_margin(egui::Margin::same(10)),
                                        |ui| {
                                            ui.set_max_width(ui.available_width());
                                            ui.horizontal(|ui| {
                                                if let Some(tex) = tex {
                                                    ui.add(egui::Image::new((
                                                        tex.id(),
                                                        egui::Vec2::splat(40.0),
                                                    )));
                                                } else {
                                                    ui.add_sized(
                                                        [40.0, 40.0],
                                                        egui::Button::new(
                                                            egui::RichText::new(initial)
                                                                .color(egui::Color32::WHITE)
                                                                .strong()
                                                                .size(16.0),
                                                        )
                                                        .fill(avatar_color(&c.track_name))
                                                        .corner_radius(11.0),
                                                    );
                                                }
                                                ui.vertical(|ui| {
                                                    ui.set_max_width(ui.available_width());
                                                    ui.add(
                                                        egui::Label::new(
                                                            egui::RichText::new(&c.track_name)
                                                                .strong(),
                                                        )
                                                        .truncate(),
                                                    );
                                                    ui.add(
                                                        egui::Label::new(
                                                            egui::RichText::new(format!(
                                                                "{} · v{} · {} · {} · {}",
                                                                c.artist_name,
                                                                c.version,
                                                                c.kind,
                                                                c.countries.join("+"),
                                                                score
                                                            ))
                                                            .color(pal.weak)
                                                            .size(11.5),
                                                        )
                                                        .truncate(),
                                                    );
                                                });
                                            });
                                        },
                                    );
                                    if resp.clicked() {
                                        self.cand_selected = Some(i);
                                        self.run_preview(&ctx);
                                    }
                                    if resp.double_clicked() {
                                        self.cand_selected = Some(i);
                                        self.apply_selected(&ctx);
                                    }
                                }
                                if self.candidates.is_empty() {
                                    ui.label(
                                        egui::RichText::new("暂无候选").color(pal.weak).size(12.0),
                                    );
                                }
                            });
                    }
                    SourceTab::Current => {}
                    SourceTab::File => {
                        if self.is_admin {
                            ui.label(
                                egui::RichText::new("管理员模式下请用选择文件导入图片")
                                    .size(11.5)
                                    .color(pal.secondary),
                            );
                        }
                        if outline_button(ui, &pal, "选择文件…").clicked() {
                            if let Some(p) = rfd::FileDialog::new()
                                .add_filter(
                                    "图片与程序图标",
                                    &[
                                        "png", "jpg", "jpeg", "webp", "bmp", "ico", "exe", "dll",
                                        "icl",
                                    ],
                                )
                                .pick_file()
                            {
                                self.choose_source_path(ctx, p);
                            }
                        }
                        if !self.file_path.is_empty() {
                            let fp = format!(
                                "已选：{}",
                                std::path::Path::new(&self.file_path)
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                            );
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&fp).color(pal.weak).size(11.5),
                                )
                                .truncate(),
                            )
                            .on_hover_text(&self.file_path);
                        }
                    }
                }
                if self.tab == SourceTab::File
                    && ui.checkbox(&mut self.file_design, "自定义编辑").changed()
                {
                    self.run_preview(ctx);
                }
                if self.tab == SourceTab::Current
                    || (self.tab == SourceTab::File && self.file_design)
                {
                    self.style_controls(ui, ctx, pal);
                }
                ui.add_space(12.0);
            });
        ui.add_space((ui.available_height() - 43.0).max(0.0));
        let can_reapply = self
            .applied
            .get(&sc.path)
            .map(|entry| entry.icon_path != sc.icon_path)
            .unwrap_or(false);
        let can_undo = self.originals.contains_key(&sc.path) && self.is_changed(&sc);
        let action_width = (ui.available_width()
            - if can_reapply { 116.0 } else { 0.0 }
            - if can_undo { 82.0 } else { 0.0 }
            - 12.0)
            .max(120.0);
        ui.horizontal_wrapped(|ui| {
            if primary_button(ui, pal, "应用图标", action_width).clicked() {
                self.apply_selected(&ctx);
            }
            if let Some(entry) = self.applied.get(&sc.path).cloned() {
                if sc.icon_path != entry.icon_path && outline_button(ui, &pal, "重新应用").clicked()
                {
                    self.start_recovery(
                        ctx,
                        crate::recovery::Action::Reapply(sc.path.clone()),
                        "重新应用",
                    );
                }
            }
            if self.originals.contains_key(&sc.path)
                && self.is_changed(&sc)
                && outline_button(ui, &pal, "撤回").clicked()
            {
                self.start_recovery(ctx, crate::recovery::Action::One(sc.path.clone()), "撤回");
            }
        });
    }

    fn compact_snapshots(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.mutation_busy() {
            ui.disable();
        }

        if self.snapshots.is_empty() {
            ui.label("暂无快照");
        }
        for snap in self.snapshots.clone() {
            ui.horizontal(|ui| {
                let mut checked = self.selected_snaps.contains(&snap.id);
                if ui.checkbox(&mut checked, "").clicked() {
                    if checked {
                        self.selected_snaps.insert(snap.id.clone());
                    } else {
                        self.selected_snaps.remove(&snap.id);
                    }
                }
                ui.add(
                    egui::Label::new(format!(
                        "{} · {} 项",
                        snapshot_time_label(&snap.id),
                        snap.count
                    ))
                    .truncate(),
                )
                .on_hover_text(&snap.id);
                if ui.button("还原").clicked() {
                    self.start_recovery(
                        ctx,
                        crate::recovery::Action::Snapshots(vec![snap.id.clone()]),
                        "快照还原",
                    );
                }
            });
        }
        if !self.snapshots.is_empty() {
            ui.horizontal(|ui| {
                if ui.button("撤销所选").clicked() {
                    let ids: Vec<String> = self.selected_snaps.iter().cloned().collect();
                    if ids.is_empty() {
                        self.status = "先勾选要撤销的快照".to_string();
                    } else {
                        self.start_recovery(
                            ctx,
                            crate::recovery::Action::Snapshots(ids),
                            "批量撤销",
                        );
                    }
                }
            });
        }
    }

    fn compact_list(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, pal: &Palette) {
        ui.horizontal(|ui| {
            ui.heading("桌面图标");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(format!("{} 个快捷方式", self.shortcuts.len()))
                        .size(12.0)
                        .color(pal.secondary),
                );
            });
        });
        if let Some(path) = self.pending_image.clone() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            egui::Frame::new()
                .fill(pal.nav_sel_bg)
                .corner_radius(12.0)
                .inner_margin(10)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [(ui.available_width() - 65.0).max(90.0), 30.0],
                            egui::Label::new(format!("已载入 {name} · 请选择应用")).truncate(),
                        )
                        .on_hover_text(name.as_ref());
                        if ui.button("取消").clicked() {
                            self.pending_image = None;
                        }
                    });
                });
        }
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let search_width = (ui.available_width() - 208.0).max(70.0);
            let (search_rect, _) =
                ui.allocate_exact_size(egui::vec2(17.0, 17.0), egui::Sense::hover());
            draw_icon(ui.painter(), search_rect, Icon::Search, pal.secondary);
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("搜索应用…")
                    .desired_width(search_width),
            );
            let old_folder = self.folder_idx;
            egui::ComboBox::from_id_salt("compact_folder")
                .width(105.0)
                .selected_text(
                    self.folders
                        .get(self.folder_idx)
                        .map(|(n, _)| n.as_str())
                        .unwrap_or("文件夹"),
                )
                .show_ui(ui, |ui| {
                    for (i, (name, _)) in self.folders.iter().enumerate() {
                        ui.selectable_value(&mut self.folder_idx, i, name);
                    }
                });
            if old_folder != self.folder_idx {
                self.reload_shortcuts(ctx.clone());
            }
            icon_menu_button(ui, &pal, |ui| {
                if ui.button("主题套装…").clicked() {
                    self.active_page = 9;
                    ui.close();
                }
                if ui.button("图标制作…").clicked() {
                    self.start_studio(ctx);
                    ui.close();
                }
                if ui.button("导入图标包…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.prepare_icon_pack(ctx, &dir);
                    }
                    ui.close();
                }
                if ui.button("其他文件夹…").clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        self.folders.retain(|(name, _)| name != "其他文件夹");
                        self.folders.push(("其他文件夹".into(), path));
                        self.folder_idx = self.folders.len() - 1;
                        self.reload_shortcuts(ctx.clone());
                    }
                    ui.close();
                }
                if ui.button("创建快捷方式").clicked() {
                    self.open_folder(ctx, crate::shell::Folder::Apps);
                    ui.close();
                }
                if ui
                    .add_enabled(
                        self.target_check_rx.is_none(),
                        egui::Button::new("检测失效项"),
                    )
                    .clicked()
                {
                    self.start_target_check(ctx);
                    ui.close();
                }
                if ui.button("刷新列表").clicked() {
                    self.reload_shortcuts(ctx.clone());
                    ui.close();
                }
            });
        });
        ui.add_space(12.0);
        if self.target_check_rx.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("正在检查本地目标…");
            });
        } else if !self.target_checks.is_empty() {
            let missing = self
                .target_checks
                .values()
                .filter(|v| matches!(v, crate::maintenance::TargetHealth::Missing))
                .count();
            ui.horizontal(|ui| {
                ui.label(format!("目标不存在 {missing} 项"));
                ui.checkbox(&mut self.only_invalid, "仅看失效项");
            });
        }
        let filter = self.filter.trim().to_lowercase();
        let visible: Vec<_> = self
            .shortcuts
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                (filter.is_empty() || s.name.to_lowercase().contains(&filter))
                    && (!self.only_invalid
                        || self.target_checks.get(&lnk::path_key(&s.path))
                            == Some(&crate::maintenance::TargetHealth::Missing))
            })
            .map(|(i, s)| (i, s.clone()))
            .collect();
        egui::ScrollArea::vertical()
            .id_salt(("compact_list", self.folder_idx))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if visible.is_empty() {
                    ui.label(if self.shortcuts.is_empty() {
                        "没有快捷方式"
                    } else {
                        "没有匹配的应用"
                    });
                }
                for (index, shortcut) in visible {
                    let changed = self.is_changed(&shortcut);
                    let (state, kind) = if self.target_checks.get(&lnk::path_key(&shortcut.path))
                        == Some(&crate::maintenance::TargetHealth::Missing)
                    {
                        ("已失效", "broken")
                    } else if self.excluded.contains(&lnk::path_key(&shortcut.path)) {
                        ("已排除", "excluded")
                    } else if changed {
                        ("已更换", "changed")
                    } else {
                        ("未更换", "none")
                    };
                    let width = ui.available_width();
                    let (rect, response) =
                        ui.allocate_exact_size(egui::vec2(width, 62.0), egui::Sense::click());
                    ui.painter().rect_filled(
                        rect,
                        14.0,
                        if response.hovered() {
                            pal.sel_fill
                        } else {
                            pal.card
                        },
                    );
                    let icon_rect = egui::Rect::from_min_size(
                        rect.min + egui::vec2(12.0, 12.0),
                        egui::vec2(38.0, 38.0),
                    );
                    if let Some(Some(texture)) = self.icons.get(index) {
                        ui.painter().image(
                            texture.id(),
                            icon_rect,
                            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                    } else {
                        ui.painter().rect_filled(icon_rect, 10.0, pal.seg_bg);
                        ui.painter().text(
                            icon_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            shortcut.name.chars().next().unwrap_or('?'),
                            egui::FontId::proportional(19.0),
                            pal.text,
                        );
                    }
                    let content_rect = egui::Rect::from_min_max(
                        rect.min + egui::vec2(62.0, 9.0),
                        rect.max - egui::vec2(12.0, 7.0),
                    );
                    let mut row = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(content_rect)
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    );
                    row.vertical(|ui| {
                        ui.set_width((width - 170.0).max(80.0));
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&shortcut.name)
                                    .size(13.5)
                                    .color(pal.text),
                            )
                            .truncate()
                            .selectable(false)
                            .sense(egui::Sense::hover()),
                        )
                        .on_hover_text(&shortcut.name);
                        let target = if shortcut.kind == "broken" {
                            "快捷方式无法解析".into()
                        } else if shortcut.target.is_empty() {
                            "UWP / 系统应用".into()
                        } else {
                            std::path::Path::new(&shortcut.target)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        };
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(
                                    if self.folder_idx == 0
                                        && lnk::is_protected_folder(std::path::Path::new(
                                            &shortcut.path,
                                        ))
                                    {
                                        format!("公共 · {target}")
                                    } else {
                                        target
                                    },
                                )
                                .size(11.5)
                                .color(pal.secondary),
                            )
                            .truncate()
                            .selectable(false)
                            .sense(egui::Sense::hover()),
                        );
                    });
                    let (bg, fg) = badge_style(self.dark_mode, kind);
                    let badge_rect = egui::Rect::from_center_size(
                        egui::pos2(rect.right() - 48.0, rect.center().y),
                        egui::vec2(52.0, 21.0),
                    );
                    ui.painter().rect_filled(badge_rect, 10.0, bg);
                    ui.painter().text(
                        badge_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        state,
                        egui::FontId::proportional(11.0),
                        fg,
                    );
                    draw_icon(
                        ui.painter(),
                        egui::Rect::from_center_size(
                            egui::pos2(rect.right() - 13.0, rect.center().y),
                            egui::vec2(14.0, 14.0),
                        ),
                        Icon::ChevronRight,
                        pal.secondary,
                    );
                    // Register the row after its children so labels cannot steal clicks.
                    let response = ui.interact(
                        rect,
                        ui.scope_id().with(("compact_row", index)),
                        egui::Sense::click(),
                    );
                    if response.hovered() {
                        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if response.clicked() {
                        self.select(ctx, index);
                    }
                    ui.add_space(6.0);
                }
            });
    }

    fn compact_settings(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, pal: &Palette) {
        ui.heading("设置");
        ui.add_space(14.0);
        egui::ScrollArea::vertical()
            .id_salt("compact_settings")
            .show(ui, |ui| {
                egui::Frame::new()
                    .fill(pal.card)
                    .corner_radius(16.0)
                    .inner_margin(16)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let (rect, _) = ui
                                .allocate_exact_size(egui::vec2(19.0, 19.0), egui::Sense::hover());
                            draw_icon(ui.painter(), rect, Icon::Palette, pal.secondary);
                            ui.label(egui::RichText::new("外观").strong());
                        });
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.label("深色模式");
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if theme_switch(ui, pal, &mut self.dark_mode).changed() {
                                        let mut config = AppConfig::load();
                                        config.dark_mode = self.dark_mode;
                                        if let Err(e) = config.save() {
                                            self.status = format!("保存失败：{e:#}");
                                        }
                                    }
                                },
                            );
                        });
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.label("透明棋盘格");
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let mut enabled = self.show_checkerboard;
                                    if theme_switch(ui, pal, &mut enabled).changed() {
                                        match AppConfig::load_checked().and_then(|mut config| {
                                            config.hide_checkerboard = !enabled;
                                            config.save()
                                        }) {
                                            Ok(()) => self.show_checkerboard = enabled,
                                            Err(e) => self.status = format!("保存失败：{e:#}"),
                                        }
                                    }
                                },
                            );
                        });
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.label("界面缩放");
                            let mut scale = ctx.zoom_factor();
                            if ui
                                .add(
                                    egui::Slider::new(&mut scale, UI_SCALE_MIN..=UI_SCALE_MAX)
                                        .show_value(false),
                                )
                                .changed()
                            {
                                ctx.set_zoom_factor(scale);
                            }
                            ui.label(format!("{:.0}%", scale * 100.0));
                            if ui.small_button("重置").clicked() {
                                ctx.set_zoom_factor(1.0);
                            }
                        });
                    });
                ui.add_space(12.0);
                let admin_label = if self.is_admin {
                    "管理员身份 · 已启用"
                } else {
                    "管理员身份 · 未启用"
                };
                let available = !self.is_admin && self.admin_block_reason().is_none();
                let reason = self
                    .admin_block_reason()
                    .unwrap_or("重启后保留当前目录与编辑参数");
                let action = ui.add_enabled_ui(available, |ui| {
                    setting_action_row(
                        ui,
                        pal,
                        Icon::ShieldCheck,
                        admin_label,
                        if self.is_admin { "已启用" } else { "启用" },
                    )
                });
                action.response.on_hover_text(reason);
                if action.inner {
                    self.start_admin(ctx);
                }
                ui.add_space(7.0);
                if setting_action_row(ui, pal, Icon::Palette, "主题套装", "管理") {
                    self.active_page = 9;
                }
                ui.add_space(7.0);
                if setting_action_row(ui, pal, Icon::AppWindow, "资源图标", "浏览") {
                    self.start_studio(ctx);
                    self.pick_resource_file(ctx);
                }
                ui.add_space(7.0);
                if setting_action_row(ui, pal, Icon::FolderOpen, "备份空间", "管理") {
                    self.active_page = 8;
                    self.refresh_backups(ctx, None);
                }
                ui.add_space(7.0);
                if setting_action_row(ui, pal, Icon::History, "快照", "查看") {
                    self.snapshots = pipeline::list_snapshots();
                    self.active_page = 4;
                    self.editing = false;
                    self.invalidate_reads();
                }
                ui.add_space(7.0);
                if setting_action_row(ui, pal, Icon::Palette, "图标制作", "打开") {
                    self.start_studio(ctx);
                }
                ui.add_space(7.0);
                if setting_action_row(ui, pal, Icon::Palette, "样式预设", "管理") {
                    self.active_page = 5;
                }
                ui.add_space(7.0);
                if setting_action_row(ui, pal, Icon::FolderOpen, "图标目录", "打开") {
                    self.open_folder(ctx, crate::shell::Folder::Icons);
                }
                ui.add_space(7.0);
                if setting_action_row(ui, pal, Icon::FileText, "操作日志", "查看") {
                    self.show_log = true;
                }
                ui.add_space(7.0);
                if setting_action_row(ui, pal, Icon::AppWindow, "应用快捷方式", "创建") {
                    self.open_folder(ctx, crate::shell::Folder::Apps);
                }
                ui.add_space(16.0);
                ui.label(
                    egui::RichText::new(concat!("IconForge v", env!("CARGO_PKG_VERSION")))
                        .size(12.0)
                        .color(pal.secondary),
                );
            });
    }
}

fn theme_switch(ui: &mut egui::Ui, pal: &Palette, on: &mut bool) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(egui::vec2(38.0, 22.0), egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::Role::CheckBox, ui.is_enabled(), *on, "深色模式")
    });
    let t = ui.ctx().animate_bool(response.id, *on);
    ui.painter()
        .rect_filled(rect, 11.0, if *on { ACCENT } else { pal.seg_bg });
    ui.painter().circle_filled(
        egui::pos2(rect.left() + 11.0 + t * 16.0, rect.center().y),
        8.0,
        if *on {
            egui::Color32::WHITE
        } else {
            pal.secondary
        },
    );
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(2.0),
            13.0,
            egui::Stroke::new(1.0, ACCENT),
            egui::StrokeKind::Outside,
        );
    }
    response.on_hover_text("深色模式")
}

fn setting_action_row(
    ui: &mut egui::Ui,
    pal: &Palette,
    icon: Icon,
    label: &str,
    action: &str,
) -> bool {
    let mut clicked = false;
    egui::Frame::new()
        .fill(pal.card)
        .corner_radius(14.0)
        .inner_margin(12)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(19.0, 19.0), egui::Sense::hover());
                draw_icon(ui.painter(), rect, icon, pal.secondary);
                let label_width = (ui.available_width() - 80.0).max(70.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(label_width, 30.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.set_min_width(label_width);
                        ui.set_max_width(label_width);
                        ui.add(egui::Label::new(egui::RichText::new(label).size(13.0)).truncate());
                    },
                );
                clicked = ui
                    .add_sized([66.0, 30.0], egui::Button::new(action).corner_radius(10.0))
                    .clicked();
            });
        });
    clicked
}

fn snapshot_time_label(id: &str) -> String {
    let Some(seconds) = id
        .strip_prefix("snap-")
        .and_then(|s| s.split('-').next())
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return id.into();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let age = now.saturating_sub(seconds);
    if age < 60 {
        "刚刚".into()
    } else if age < 3600 {
        format!("{} 分钟前", age / 60)
    } else if age < 86400 {
        format!("{} 小时前", age / 3600)
    } else {
        format!("{} 天前", age / 86400)
    }
}

impl App {
    fn style_controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, pal: &Palette) {
        self.preset_picker(ui, ctx, pal);
        ui.add_space(10.0);
        ui.label(egui::RichText::new("底板").color(pal.secondary).size(12.5));
        let base_idx = match self.base.as_str() {
            "pick" => 2,
            "pure" => 1,
            s if s.starts_with("custom:") => 3,
            s if s.starts_with("linear:") => 4,
            _ => 0,
        };
        if let Some(i) = segmented(
            ui,
            &pal,
            &["白底", "纯白", "取色", "自定义", "渐变"],
            base_idx,
        ) {
            self.base = match i {
                1 => "pure".to_string(),
                2 => "pick".to_string(),
                3 => crate::gradient::custom_name(self.custom_base),
                4 => self.gradient_draft.name(),
                _ => "white".to_string(),
            };
            self.run_preview(&ctx);
        }
        if self.base.starts_with("custom:") {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("自定义颜色")
                        .color(pal.secondary)
                        .size(12.5),
                );
                let mut c32 = egui::Color32::from_rgb(
                    self.custom_base.r,
                    self.custom_base.g,
                    self.custom_base.b,
                );
                if ui.color_edit_button_srgba(&mut c32).changed() {
                    self.custom_base = crate::gradient::Rgb::new(c32.r(), c32.g(), c32.b());
                    self.base = crate::gradient::custom_name(self.custom_base);
                    self.run_preview(&ctx);
                }
            });
        }
        self.gradient_controls(ui, ctx);
        egui::CollapsingHeader::new("微调")
            .id_salt("compact_tuning")
            .default_open(false)
            .show(ui, |ui| {
                // Uniform 3-column grid for the 4 sliders so all
                // tracks and right-aligned percentages line up.
                ui.add_space(8.0);
                let avail_row_w = ui.available_width();
                let label_col_w = 50.0;
                let pct_col_w = 38.0;
                let slider_track_w = (avail_row_w - label_col_w - pct_col_w - 28.0).max(60.0);
                let mut any_slider_changed = false;
                ui.scope(|ui| {
                    // Darker, high-contrast track + terracotta filled bar + crisp handle
                    let vis = ui.visuals_mut();
                    vis.slider_trailing_fill = true;
                    vis.selection.bg_fill = ACCENT;
                    if self.dark_mode {
                        vis.widgets.inactive.bg_fill = egui::Color32::from_rgb(0x4E, 0x4C, 0x44);
                        vis.widgets.inactive.bg_stroke =
                            egui::Stroke::new(1.0, egui::Color32::from_rgb(0x6B, 0x68, 0x5E));
                        vis.widgets.inactive.fg_stroke =
                            egui::Stroke::new(2.0, egui::Color32::from_rgb(0xFF, 0xFF, 0xFF));
                    } else {
                        vis.widgets.inactive.bg_fill = egui::Color32::from_rgb(0xCF, 0xCA, 0xB8);
                        vis.widgets.inactive.bg_stroke =
                            egui::Stroke::new(1.0, egui::Color32::from_rgb(0xB5, 0xAF, 0x9C));
                        vis.widgets.inactive.fg_stroke = egui::Stroke::new(2.0, ACCENT);
                    }
                    vis.widgets.hovered.fg_stroke = egui::Stroke::new(2.5, ACCENT);
                    vis.widgets.active.fg_stroke = egui::Stroke::new(2.5, egui::Color32::WHITE);

                    egui::Grid::new("detail_sliders_grid")
                        .num_columns(3)
                        .min_col_width(0.0)
                        .spacing([6.0, 7.0])
                        .show(ui, |ui| {
                            let mut row =
                                |label: &str,
                                 val: &mut f32,
                                 range: std::ops::RangeInclusive<f32>,
                                 default_val: f32,
                                 pct_scale: f32| {
                                    ui.add_sized(
                                        [label_col_w, 20.0],
                                        egui::Label::new(
                                            egui::RichText::new(label)
                                                .color(pal.secondary)
                                                .size(12.0),
                                        ),
                                    );
                                    ui.spacing_mut().slider_width = slider_track_w;
                                    let r = ui.add(
                                        egui::Slider::new(val, range)
                                            .show_value(false)
                                            .trailing_fill(true),
                                    );
                                    if r.double_clicked() {
                                        *val = default_val;
                                        any_slider_changed = true;
                                    } else if r.changed() {
                                        any_slider_changed = true;
                                    }
                                    ui.allocate_ui_with_layout(
                                        egui::Vec2::new(pct_col_w, 20.0),
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "{:.0}%",
                                                    *val * pct_scale
                                                ))
                                                .color(pal.text)
                                                .size(12.0),
                                            );
                                        },
                                    );
                                    ui.end_row();
                                };

                            row(
                                "图标缩放",
                                &mut self.glyph_scale,
                                0.5..=1.2,
                                crate::composite::DEFAULT_GLYPH_SCALE,
                                100.0,
                            );
                            row(
                                "图标圆角",
                                &mut self.icon_radius,
                                0.0..=0.5,
                                crate::composite::DEFAULT_ICON_RADIUS,
                                100.0,
                            );
                            row(
                                "底板大小",
                                &mut self.base_size,
                                0.6..=1.0,
                                crate::composite::DEFAULT_BASE_SIZE,
                                100.0,
                            );
                            row(
                                "底板圆角",
                                &mut self.base_radius,
                                0.0..=100.0,
                                crate::composite::exponent_to_radius_pct(
                                    crate::composite::DEFAULT_BASE_EXPONENT,
                                ),
                                1.0,
                            );
                        });
                });
                if any_slider_changed {
                    self.run_preview(&ctx);
                }
            });
    }
    fn open_folder(&mut self, ctx: &egui::Context, folder: crate::shell::Folder) {
        if self.folder_rx.is_some() {
            self.status = "正在打开文件夹，请稍候…".into();
            return;
        }
        let name = match folder {
            crate::shell::Folder::Apps => "应用快捷方式",
            crate::shell::Folder::Icons => "图标目录",
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.folder_rx = Some(rx);
        self.status = format!("正在打开{name}…");
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = crate::shell::open_folder(folder)
                .map(|_| format!("已打开{name}"))
                .map_err(|e| format!("打开{name}失败：{e:#}"));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
    fn poll_folder(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.folder_rx.as_ref() else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
                return;
            }
            Err(_) => Err("打开文件夹任务中断，请重试".into()),
        };
        self.folder_rx = None;
        self.status = match result {
            Ok(message) => message,
            Err(error) => {
                self.log_push(&error);
                error
            }
        };
    }
    fn admin_block_reason(&self) -> Option<&'static str> {
        if self.active_page == 7 {
            Some("请先返回图标列表再启用管理员")
        } else if self.admin_rx.is_some() {
            Some("正在等待 Windows 授权")
        } else if self.mutation_busy() || self.webdav_rx.is_some() {
            Some("请等待当前操作完成")
        } else if self.active_page == 6 || self.pack_scan_rx.is_some() {
            Some("请先完成或返回退出图标包预览")
        } else if self.draft_style.is_some()
            || self.pending_image.is_some()
            || self.webdav_download.is_some()
        {
            Some("请先完成当前预览或编辑")
        } else {
            None
        }
    }
    fn start_admin(&mut self, ctx: &egui::Context) {
        if self.is_admin {
            self.status = "当前已是管理员身份".into();
            return;
        }
        if let Some(reason) = self.admin_block_reason() {
            self.status = reason.into();
            return;
        }
        let state = crate::elevation::ResumeState {
            directory: self.folders.get(self.folder_idx).map(|(_, p)| p.clone()),
            selected: self.selected_shortcut().map(|s| s.path.clone()),
            filter: self.filter.clone(),
            page: self.active_page,
            editing: self.editing,
            style: self.current_style("重启编辑"),
            file: (self.tab == SourceTab::File).then(|| self.file_path.clone()),
            pending_image: self.pending_image.clone(),
            search_query: self.search_query.clone(),
            file_design: self.file_design,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.admin_rx = Some(rx);
        self.status = "等待 Windows 授权…".into();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = crate::elevation::restart(&state).map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
    fn poll_admin(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.admin_rx.as_ref() else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
                return;
            }
            Err(_) => Err("管理员启动任务中断，请重试".into()),
        };
        self.admin_rx = None;
        match result {
            Ok(crate::elevation::Outcome::Started) => {
                self.status = "管理员窗口已启动".into();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Ok(crate::elevation::Outcome::Cancelled) => {
                self.status = "已取消管理员授权，可继续使用当前窗口".into()
            }
            Err(error) => self.status = error,
        }
    }
    fn restore_resume(&mut self, ctx: &egui::Context, state: crate::elevation::ResumeState) {
        if let Some(directory) = state.directory {
            self.folder_idx =
                if let Some(index) = self.folders.iter().position(|(_, p)| *p == directory) {
                    index
                } else {
                    self.folders.push(("其他文件夹".into(), directory));
                    self.folders.len() - 1
                };
        }
        self.reload_shortcuts(ctx.clone());
        if let Some(index) = state
            .selected
            .as_ref()
            .and_then(|path| self.shortcuts.iter().position(|s| &s.path == path))
        {
            self.select(ctx, index);
        }
        self.use_style(ctx, &state.style);
        self.search_query = state.search_query;
        if let Some(file) = state.file {
            self.file_design = state.file_design;
            self.file_path = file;
            self.tab = SourceTab::File;
            self.run_preview(ctx);
        }
        self.filter = state.filter;
        self.pending_image = state.pending_image;
        self.active_page = state.page;
        if self.active_page == 4 {
            self.snapshots = pipeline::list_snapshots();
        }
        self.editing = state.editing && self.selected_shortcut().is_some();
        self.status = if self.is_admin {
            "已启用管理员身份"
        } else {
            "当前仍为普通身份，请重新启用管理员"
        }
        .into();
    }
    fn current_style(&self, name: &str) -> StylePreset {
        StylePreset {
            name: name.into(),
            base: self.base.clone(),
            glyph_scale: self.glyph_scale,
            icon_radius: self.icon_radius,
            base_size: self.base_size,
            base_exponent: crate::composite::radius_pct_to_exponent(self.base_radius),
        }
    }
    fn store_presets(&mut self, presets: Vec<StylePreset>) -> anyhow::Result<()> {
        for preset in &presets {
            preset.validate()?;
        }
        let mut config = AppConfig::load();
        config.style_presets = presets.clone();
        config.save()?;
        self.style_presets = presets;
        Ok(())
    }
    fn use_style(&mut self, ctx: &egui::Context, preset: &StylePreset) {
        if let Err(e) = preset.validate() {
            self.status = e.to_string();
            return;
        }
        self.base = preset.base.clone();
        if let Some(color) = crate::gradient::parse_custom(&self.base) {
            self.custom_base = color;
        }
        self.glyph_scale = preset.glyph_scale;
        self.icon_radius = preset.icon_radius;
        self.base_size = preset.base_size;
        self.base_radius = crate::composite::exponent_to_radius_pct(preset.base_exponent);
        if self.tab != SourceTab::File {
            self.tab = SourceTab::Current;
        }
        self.run_preview(ctx);
    }
    fn preset_picker(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, _pal: &Palette) {
        let current = self.current_style("");
        let selected = self
            .style_presets
            .iter()
            .position(|p| p.same_style(&current));
        let title = selected
            .map(|i| self.style_presets[i].name.clone())
            .unwrap_or_else(|| "自定义".into());
        let mut choice = None;
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("样式");
            egui::ComboBox::from_id_salt("style_picker")
                .width((ui.available_width() - 80.0).max(110.0))
                .selected_text(&title)
                .show_ui(ui, |ui| {
                    if ui.selectable_label(false, "恢复默认参数").clicked() {
                        choice = Some(StylePreset::default());
                    }
                    for (i, preset) in self.style_presets.iter().enumerate() {
                        if ui
                            .selectable_label(selected == Some(i), &preset.name)
                            .clicked()
                        {
                            choice = Some(preset.clone());
                        }
                    }
                });
            if ui.button("保存").clicked() {
                self.draft_style = Some(self.current_style(if selected.is_some() {
                    &title
                } else {
                    "新样式"
                }));
                self.preset_error.clear();
            }
        });
        if let Some(preset) = choice {
            self.use_style(ctx, &preset);
        }
    }
    fn preset_save_window(&mut self, ctx: &egui::Context) {
        let Some(mut draft) = self.draft_style.clone() else {
            return;
        };
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        egui::Window::new("保存样式")
            .open(&mut open)
            .resizable(false)
            .default_width(300.0)
            .show(ctx, |ui| {
                if self.mutation_busy() {
                    ui.disable();
                }
                ui.label("名称");
                ui.add(egui::TextEdit::singleline(&mut draft.name).desired_width(f32::INFINITY));
                let exists = self
                    .style_presets
                    .iter()
                    .any(|p| p.name.trim().to_lowercase() == draft.name.trim().to_lowercase());
                if !self.preset_error.is_empty() {
                    ui.colored_label(ACCENT, &self.preset_error);
                }
                ui.horizontal(|ui| {
                    if ui
                        .button(if exists {
                            "更新同名预设"
                        } else {
                            "保存预设"
                        })
                        .clicked()
                    {
                        save = true;
                    }
                    if ui.button("取消").clicked() {
                        cancel = true;
                    }
                });
            });
        if save {
            draft.name = draft.name.trim().into();
            let result = (|| -> anyhow::Result<()> {
                draft.validate()?;
                let mut presets = self.style_presets.clone();
                if let Some(index) = presets
                    .iter()
                    .position(|p| p.name.to_lowercase() == draft.name.to_lowercase())
                {
                    presets[index] = draft.clone();
                } else {
                    presets.push(draft.clone());
                }
                self.store_presets(presets)
            })();
            match result {
                Ok(()) => {
                    self.status = format!("已保存样式：{}", draft.name);
                    self.draft_style = None;
                    return;
                }
                Err(e) => self.preset_error = format!("保存失败：{e:#}"),
            }
        }
        self.draft_style = if open && !cancel { Some(draft) } else { None };
    }
    fn preset_manager(&mut self, ui: &mut egui::Ui, _ctx: &egui::Context, pal: &Palette) {
        ui.horizontal(|ui| {
            if icon_button(ui, pal, Icon::ArrowLeft, "返回", None, false).clicked() {
                self.active_page = 3;
            }
            ui.heading("样式预设");
        });
        ui.add_space(12.0);
        egui::ScrollArea::vertical()
            .id_salt("presets_manager")
            .show(ui, |ui| {
                if self.style_presets.is_empty() {
                    ui.label("暂无预设");
                }
                for preset in self.style_presets.clone() {
                    egui::Frame::new()
                        .fill(pal.card)
                        .corner_radius(14.0)
                        .inner_margin(12)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let text = format!(
                                    "{} · 缩放 {:.0}%",
                                    preset.name,
                                    preset.glyph_scale * 100.0
                                );
                                ui.add_sized(
                                    [(ui.available_width() - 80.0).max(80.0), 30.0],
                                    egui::Label::new(text).truncate(),
                                )
                                .on_hover_text(format!(
                                    "{} · 底板大小 {:.0}%",
                                    preset.base,
                                    preset.base_size * 100.0
                                ));
                                if ui.button("删除").clicked() {
                                    self.delete_preset = Some(preset.name.clone());
                                }
                            });
                        });
                    ui.add_space(7.0);
                }
                if let Some(name) = self.delete_preset.clone() {
                    ui.separator();
                    ui.label(format!("删除预设“{name}”？"));
                    ui.horizontal(|ui| {
                        if ui.button("确认删除").clicked() {
                            let presets = self
                                .style_presets
                                .iter()
                                .filter(|p| p.name != name)
                                .cloned()
                                .collect();
                            match self.store_presets(presets) {
                                Ok(()) => {
                                    self.delete_preset = None;
                                    self.status = "预设已删除".into();
                                }
                                Err(e) => self.status = format!("删除失败：{e:#}"),
                            }
                        }
                        if ui.button("取消").clicked() {
                            self.delete_preset = None;
                        }
                    });
                }
            });
    }
    fn handle_image_drop(&mut self, ctx: &egui::Context) {
        let files = ctx.input(|i| i.raw.dropped_files.clone());
        if files.is_empty() {
            return;
        }
        if self.mutation_busy() || self.draft_style.is_some() {
            self.status = "请先完成当前操作再拖入图片".into();
            return;
        }
        if files.len() != 1 {
            self.status = "请一次拖入一张图片".into();
            return;
        }
        let path = files[0].path().to_path_buf();
        if !path.is_file() || !crate::icon_pack::supported_image(&path) {
            self.status = "支持 PNG、JPG、WEBP、BMP 和 ICO 图片".into();
            return;
        }
        self.invalidate_reads();
        if self.active_page == 7 {
            self.studio_file = path.to_string_lossy().into_owned();
            self.run_preview(ctx);
            return;
        }
        if self.active_page == 6 {
            if let Some(index) = self.pack_selected {
                if let Some(row) = self.pack_plan.as_mut().and_then(|p| p.rows.get_mut(index)) {
                    row.choose_file(path);
                    self.request_pack_preview(ctx, index);
                    return;
                }
            }
        }
        if self.active_page == 0 && self.editing && self.selected_shortcut().is_some() {
            self.pending_image = None;
            self.file_path = path.to_string_lossy().into_owned();
            self.tab = SourceTab::File;
            self.run_preview(ctx);
        } else {
            self.pending_image = Some(path);
            self.active_page = 0;
            self.editing = false;
            self.status = "图片已载入，请选择要更换的应用".into();
        }
    }
    fn request_pack_preview(&mut self, ctx: &egui::Context, index: usize) {
        self.pack_preview_rx = None;
        self.pack_after = None;
        let Some(row) = self.pack_plan.as_ref().and_then(|p| p.rows.get(index)) else {
            return;
        };
        self.pack_before = self
            .shortcuts
            .iter()
            .position(|s| s.path == row.shortcut.path)
            .and_then(|i| self.icons.get(i).cloned().flatten());
        let Some(candidate) = row.candidate() else {
            self.status = "请为此应用选择图片".into();
            return;
        };
        let source = SourceSpec {
            kind: "file".into(),
            path: candidate.path.to_string_lossy().into_owned(),
            artwork_url: String::new(),
            bundle_id: String::new(),
        };
        let shortcut = row.shortcut.path.clone();
        let style = self.current_style("preview");
        let (tx, rx) = std::sync::mpsc::channel();
        self.pack_preview_rx = Some(rx);
        self.status = "正在生成匹配预览…".into();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = pipeline::compose_preview(
                &source,
                &shortcut,
                &style.base,
                false,
                style.glyph_scale,
                style.icon_radius,
                style.base_size,
                style.base_exponent,
            )
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
    fn pack_item(&self, row: &crate::icon_pack::PackRow) -> Option<ApplyItem> {
        let candidate = row.candidate()?;
        Some(ApplyItem {
            lnk_path: row.shortcut.path.clone(),
            source: SourceSpec {
                kind: "file".into(),
                path: candidate.path.to_string_lossy().into_owned(),
                artwork_url: String::new(),
                bundle_id: String::new(),
            },
            base: self.base.clone(),
            glyph_scale: self.glyph_scale,
            icon_radius: self.icon_radius,
            base_size: self.base_size,
            base_exponent: crate::composite::radius_pct_to_exponent(self.base_radius),
        })
    }
    fn pack_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, pal: &Palette) {
        if let Some(index) = self.pack_selected {
            self.pack_detail(ui, ctx, pal, index);
            return;
        }
        let mut cancel = false;
        ui.horizontal(|ui| {
            cancel = icon_button(ui, pal, Icon::ArrowLeft, "返回", None, false).clicked();
            ui.heading("图标包");
        });
        if cancel {
            self.pack_plan = None;
            self.pack_preview_rx = None;
            self.active_page = 0;
            return;
        }
        let Some(plan) = self.pack_plan.as_mut() else {
            ui.label("暂无图标包");
            return;
        };
        ui.add_space(6.0);
        ui.label(format!(
            "匹配 {} / {} 项 · 已选 {} 项",
            plan.matched_count(),
            plan.rows.len(),
            plan.selected_count()
        ));
        ui.horizontal_wrapped(|ui| {
            if ui.button("仅选精确匹配").clicked() {
                for row in &mut plan.rows {
                    row.enabled = row.quality == Quality::Exact;
                }
            }
            if ui.button("全不选").clicked() {
                for row in &mut plan.rows {
                    row.enabled = false;
                }
            }
        });
        ui.add_space(8.0);
        let mut preview = None;
        let height = (ui.available_height() - 54.0).max(60.0);
        egui::ScrollArea::vertical()
            .id_salt("pack_rows")
            .max_height(height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width());
                for (index, row) in plan.rows.iter_mut().enumerate() {
                    egui::Frame::new()
                        .fill(pal.card)
                        .corner_radius(13.0)
                        .inner_margin(10)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.add_enabled(
                                    row.candidate().is_some(),
                                    egui::Checkbox::without_text(&mut row.enabled),
                                );
                                let label = match row.quality {
                                    Quality::Exact => "精确",
                                    Quality::Approximate => "近似",
                                    Quality::Ambiguous => "重名",
                                    Quality::Unmatched => "未匹配",
                                    Quality::Manual => "手动",
                                };
                                let detail = row
                                    .candidate()
                                    .map(|c| c.name.as_str())
                                    .unwrap_or("选择图片");
                                ui.vertical(|ui| {
                                    ui.set_width((ui.available_width() - 75.0).max(80.0));
                                    ui.add(
                                        egui::Label::new(format!(
                                            "{} · {label}",
                                            row.shortcut.name
                                        ))
                                        .truncate(),
                                    )
                                    .on_hover_text(&row.shortcut.name);
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(detail)
                                                .size(11.5)
                                                .color(pal.secondary),
                                        )
                                        .truncate(),
                                    );
                                });
                                if ui.button("预览").clicked() {
                                    preview = Some(index);
                                }
                            });
                        });
                    ui.add_space(6.0);
                }
            });
        let count = self
            .pack_plan
            .as_ref()
            .map(|p| p.selected_count())
            .unwrap_or(0);
        ui.add_space((ui.available_height() - 43.0).max(0.0));
        let clicked = ui
            .add_enabled_ui(count > 0 && !self.mutation_busy(), |ui| {
                primary_button(
                    ui,
                    pal,
                    &format!("应用所选（{count}）"),
                    ui.available_width(),
                )
                .clicked()
            })
            .inner;
        if let Some(index) = preview {
            self.pack_selected = Some(index);
            self.request_pack_preview(ctx, index);
        }
        if clicked {
            let items: Vec<_> = self
                .pack_plan
                .as_ref()
                .unwrap()
                .rows
                .iter()
                .filter(|r| r.enabled)
                .filter_map(|r| self.pack_item(r))
                .collect();
            self.pack_plan = None;
            self.pack_selected = None;
            self.pack_preview_rx = None;
            self.active_page = 0;
            self.editing = false;
            self.apply(ctx, items);
        }
    }
    fn pack_detail(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, pal: &Palette, index: usize) {
        let Some(row) = self
            .pack_plan
            .as_ref()
            .and_then(|p| p.rows.get(index))
            .cloned()
        else {
            self.pack_selected = None;
            return;
        };
        let mut back = false;
        ui.horizontal(|ui| {
            back = icon_button(ui, pal, Icon::ArrowLeft, "返回", None, false).clicked();
            ui.add(
                egui::Label::new(egui::RichText::new(&row.shortcut.name).strong().size(16.0))
                    .truncate(),
            );
        });
        if back {
            self.pack_selected = None;
            self.pack_preview_rx = None;
            return;
        }
        ui.add_space(12.0);
        egui::ScrollArea::vertical()
            .id_salt("pack_detail")
            .show(ui, |ui| {
                egui::Frame::new()
                    .fill(pal.card)
                    .corner_radius(16.0)
                    .inner_margin(16)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.columns(2, |columns| {
                            for (column, (label, texture)) in [
                                ("当前", self.pack_before.as_ref()),
                                ("预览", self.pack_after.as_ref()),
                            ]
                            .into_iter()
                            .enumerate()
                            {
                                columns[column].vertical_centered(|ui| {
                                    if let Some(texture) = texture {
                                        checker_image(
                                            ui,
                                            texture,
                                            egui::vec2(90.0, 90.0),
                                            self.dark_mode,
                                            self.show_checkerboard,
                                        );
                                    } else {
                                        let (rect, _) = ui.allocate_exact_size(
                                            egui::vec2(90.0, 90.0),
                                            egui::Sense::hover(),
                                        );
                                        ui.painter().rect_filled(rect, 16.0, pal.seg_bg);
                                    }
                                    ui.label(label);
                                });
                            }
                        });
                    });
                ui.add_space(12.0);
                ui.label("匹配图片");
                let mut choice = row.choice.unwrap_or(0);
                egui::ComboBox::from_id_salt("pack_candidate")
                    .width((ui.available_width() - 20.0).max(120.0))
                    .selected_text(row.candidate().map(|c| c.name.as_str()).unwrap_or("未匹配"))
                    .show_ui(ui, |ui| {
                        for (i, candidate) in row.candidates.iter().enumerate() {
                            ui.selectable_value(&mut choice, i, &candidate.name);
                        }
                    });
                if row.choice != Some(choice) && !row.candidates.is_empty() {
                    if let Some(r) = self.pack_plan.as_mut().and_then(|p| p.rows.get_mut(index)) {
                        r.choice = Some(choice);
                        r.enabled = true;
                        r.quality = Quality::Manual;
                    }
                    self.request_pack_preview(ctx, index);
                }
                ui.add_space(8.0);
                if outline_button(ui, pal, "选择文件…").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("图片", &["ico", "png", "jpg", "jpeg", "webp", "bmp"])
                        .pick_file()
                    {
                        if let Some(r) = self.pack_plan.as_mut().and_then(|p| p.rows.get_mut(index))
                        {
                            r.choose_file(path);
                        }
                        self.request_pack_preview(ctx, index);
                    }
                }
                ui.add_space(10.0);
                if let Some(r) = self.pack_plan.as_mut().and_then(|p| p.rows.get_mut(index)) {
                    let available = r.candidate().is_some();
                    ui.add_enabled(available, egui::Checkbox::new(&mut r.enabled, "应用此项"));
                }
            });
    }
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.flush_studio_session();
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_recovery(&ctx);
        self.poll_export(&ctx);
        self.poll_folder(&ctx);
        self.poll_admin(&ctx);
        self.poll_jobs(&ctx);
        self.poll_maintenance(&ctx);
        self.poll_assets(&ctx);
        self.ensure_history();
        self.history_hotkeys(&ctx);
        self.handle_image_drop(&ctx);
        if self.mutation_busy() {
            // Serialize writes and prevent a close from interrupting the batch.
            ui.disable();
            if ctx.input(|i| i.viewport().close_requested()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
        }
        ui.ctx().set_visuals(theme_visuals(self.dark_mode));
        let pal = Palette::of(self.dark_mode);

        // ----- UI zoom: Ctrl+wheel (Ctrl +/-/0 is handled by egui itself) -----
        // Whatever the source, the context's zoom factor is the truth: clamp
        // it, and persist it to config.json whenever it changes.
        let wheel_dy = ctx.input_mut(|i| {
            if i.modifiers.command && i.smooth_scroll_delta.y != 0.0 {
                let dy = i.smooth_scroll_delta.y;
                // Don't let the grid scroll while zooming.
                i.smooth_scroll_delta = egui::Vec2::ZERO;
                dy
            } else {
                0.0
            }
        });
        // Scroll is smoothed over several frames: accumulate it and step
        // the zoom once per ~notch instead of once per frame.
        let acc_id = egui::Id::unique("zoom_wheel_acc");
        let mut acc = ctx.data(|d| d.get_temp::<f32>(acc_id)).unwrap_or(0.0) + wheel_dy;
        let mut wheel_step = 0.0_f32;
        if acc.abs() >= 30.0 {
            wheel_step = acc.signum();
            acc = 0.0;
        } else if wheel_dy == 0.0 {
            acc = 0.0;
        }
        ctx.data_mut(|d| d.insert_temp(acc_id, acc));
        let mut zoom = ctx.zoom_factor() + 0.05 * wheel_step;
        zoom = ((zoom * 20.0).round() / 20.0).clamp(UI_SCALE_MIN, UI_SCALE_MAX);
        if (zoom - ctx.zoom_factor()).abs() > 1e-3 {
            ctx.set_zoom_factor(zoom);
        }
        if (zoom - self.ui_scale).abs() > 1e-3 {
            self.ui_scale = zoom;
            let mut cfg = AppConfig::load();
            cfg.ui_scale = zoom;
            let _ = cfg.save();
        }

        // Drain finished artwork downloads: decode + round corners +
        // upload as textures (texture creation must happen on the UI
        // thread). Failed downloads simply keep the letter avatar.
        if let Some(rx) = &self.artwork_rx {
            while let Ok((url, bytes)) = rx.try_recv() {
                if let Ok(img) = image::load_from_memory(&bytes) {
                    let mut rgba = img.to_rgba8();
                    let (w, h) = rgba.dimensions();
                    if w > 96 || h > 96 {
                        rgba = image::imageops::resize(
                            &rgba,
                            96,
                            96,
                            image::imageops::FilterType::Triangle,
                        );
                    }
                    let rgba = pipeline::rounded_thumbnail(rgba);
                    let (w, h) = rgba.dimensions();
                    let color =
                        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                    let tex = ctx.load_texture(url.clone(), color, egui::TextureOptions::default());
                    self.cand_textures.insert(url, tex);
                }
            }
        }

        if !self.mutation_busy()
            && self.active_page == 7
            && ctx.input(|i| i.key_pressed(egui::Key::Escape))
        {
            self.active_page = 0;
            self.editing = false;
            self.invalidate_reads();
        }
        if !self.mutation_busy() && self.editing && ctx.input(|i| i.key_pressed(egui::Key::Escape))
        {
            self.editing = false;
            self.invalidate_reads();
        }
        egui::Panel::bottom("compact_status")
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(pal.bg)
                    .inner_margin(egui::Margin::symmetric(18, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if let Some((done, total)) = self.recovery_progress {
                        ui.add(
                            egui::ProgressBar::new(if total == 0 {
                                0.0
                            } else {
                                done as f32 / total as f32
                            })
                            .desired_width(72.0),
                        );
                    }
                    if self.search_rx.is_some()
                        || self.preview_rx.is_some()
                        || self.mutation_busy()
                        || self.webdav_rx.is_some()
                        || self.pack_scan_rx.is_some()
                        || self.pack_preview_rx.is_some()
                    {
                        ui.spinner();
                    }
                    self.status_feedback(ui, &ctx, &pal);
                });
            });
        egui::Panel::top("compact_navigation")
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(pal.bg)
                    .inner_margin(egui::Margin::symmetric(16, 10)),
            )
            .show(ui, |ui| {
                ui.columns(4, |columns| {
                    for (index, (label, icon)) in [
                        ("图标", Icon::LayoutGrid),
                        ("一键恢复", Icon::RotateCw),
                        ("云备份", Icon::Cloud),
                        ("设置", Icon::SlidersHorizontal),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        let col = &mut columns[index];
                        let selected = self.active_page == index
                            || (index == 3 && matches!(self.active_page, 4 | 5 | 8 | 9))
                            || (index == 0 && matches!(self.active_page, 6 | 7));
                        let (response, _) = clickable_frame(
                            col,
                            egui::Frame::new()
                                .fill(if selected {
                                    pal.nav_sel_bg
                                } else {
                                    egui::Color32::TRANSPARENT
                                })
                                .corner_radius(13.0)
                                .inner_margin(egui::Margin::symmetric(8, 9)),
                            |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal_centered(|ui| {
                                    let color = if selected {
                                        pal.nav_sel_fg
                                    } else {
                                        pal.secondary
                                    };
                                    let (rect, _) = ui.allocate_exact_size(
                                        egui::vec2(17.0, 17.0),
                                        egui::Sense::hover(),
                                    );
                                    draw_icon(ui.painter(), rect, icon, color);
                                    ui.label(egui::RichText::new(label).size(13.0).color(color));
                                });
                            },
                        );
                        if response.clicked() {
                            if index == 1 {
                                self.invalidate_reads();
                                self.restore_all_from_cache(&ctx);
                            } else {
                                self.active_page = index;
                                self.editing = false;
                                self.invalidate_reads();
                            }
                        }
                    }
                });
            });
        egui::CentralPanel::default_margins()
            .frame(
                egui::Frame::new()
                    .fill(pal.bg)
                    .inner_margin(egui::Margin::symmetric(20, 10)),
            )
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width());
                if self.needs_admin && !self.is_admin && self.active_page == 0 {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new("修改此位置需要管理员权限")
                                .small()
                                .color(pal.secondary),
                        );
                        if ui.small_button("以管理员身份重启").clicked() {
                            self.start_admin(&ctx);
                        }
                    });
                    ui.add_space(8.0);
                }
                match self.active_page {
                    9 => self.theme_page(ui, &ctx, &pal),
                    8 => self.backup_page(ui, &ctx, &pal),
                    7 => self.studio_page(ui, &ctx, &pal),
                    0 if self.editing => self.compact_editor(ui, &ctx, &pal),
                    0 => self.compact_list(ui, &ctx, &pal),
                    4 => {
                        ui.horizontal(|ui| {
                            if icon_button(ui, &pal, Icon::ArrowLeft, "返回", None, false).clicked()
                            {
                                self.active_page = 3;
                            }
                            ui.heading("快照");
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    icon_menu_button(ui, &pal, |ui| {
                                        if ui.button("还原全部原始图标").clicked() {
                                            self.start_recovery(
                                                &ctx,
                                                crate::recovery::Action::All,
                                                "还原全部",
                                            );
                                            ui.close();
                                        }
                                    });
                                },
                            );
                        });
                        ui.add_space(12.0);
                        egui::ScrollArea::vertical()
                            .id_salt("compact_snapshots")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                self.compact_snapshots(ui, &ctx);
                            });
                    }
                    2 => {
                        ui.heading("云备份");
                        ui.add_space(12.0);
                        self.webdav_page(ui, &ctx);
                    }
                    5 => self.preset_manager(ui, &ctx, &pal),
                    6 => self.pack_page(ui, &ctx, &pal),
                    _ => self.compact_settings(ui, &ctx, &pal),
                }
            });
        self.resource_dialog(&ctx, &pal);
        // ----- log window (LinkEcho-basis Log tab) -----
        let mut log_open = self.show_log;
        egui::Window::new("日志")
            .open(&mut log_open)
            .show(&ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .show(ui, |ui| {
                        if self.log.is_empty() {
                            ui.label("暂无记录");
                        }
                        for line in &self.log {
                            ui.label(line);
                        }
                    });
                if ui.button("清理日志").clicked() {
                    self.log.clear();
                }
            });
        self.show_log = log_open;
        self.preset_save_window(&ctx);
        self.observe_editing(&ctx);
        self.sync_studio_session(&ctx);
        self.autosave_studio(&ctx);
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Tooltip,
                egui::Id::unique("image_drop_hint"),
            ));
            let rect = ui.max_rect().shrink(10.0);
            painter.rect_filled(rect, 18.0, pal.card.gamma_multiply(0.95));
            painter.rect_stroke(
                rect,
                18.0,
                egui::Stroke::new(2.0, ACCENT),
                egui::StrokeKind::Inside,
            );
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                if self.active_page == 7 || self.editing || self.pack_selected.is_some() {
                    "松开图片，生成预览"
                } else {
                    "松开图片，再选择应用"
                },
                egui::FontId::proportional(18.0),
                pal.text,
            );
        }
    }
}
impl App {
    fn start_studio(&mut self, ctx: &egui::Context) {
        self.invalidate_reads();
        self.after = None;
        self.active_page = 7;
        self.editing = false;
        self.tab = SourceTab::File;
        self.history_scope = None;
        let state = self.studio_session.state.clone();
        self.apply_edit_state(ctx, &state);
        self.status = if self.studio_file.is_empty() {
            "选择图片开始制作"
        } else {
            "正在恢复制作预览…"
        }
        .into();
        self.sync_studio_session(ctx);
    }
    fn studio_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, pal: &Palette) {
        let mut back = false;
        ui.horizontal(|ui| {
            back = icon_button(ui, pal, Icon::ArrowLeft, "返回", None, false).clicked();
            ui.heading("图标制作");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.history_buttons(ui, ctx, pal, true)
            });
        });
        if back {
            self.active_page = 0;
            self.invalidate_reads();
            return;
        }
        ui.add_space(10.0);
        egui::ScrollArea::vertical()
            .id_salt("studio_scroll")
            .max_height((ui.available_height() - 55.0).max(90.0))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width());
                egui::Frame::new()
                    .fill(pal.seg_bg)
                    .corner_radius(16.0)
                    .inner_margin(12)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.vertical_centered(|ui| {
                            if let Some(texture) = &self.after {
                                checker_image(
                                    ui,
                                    texture,
                                    egui::vec2(136.0, 136.0),
                                    self.dark_mode,
                                    self.show_checkerboard,
                                );
                            } else {
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(136.0, 136.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    "预览",
                                    egui::FontId::proportional(13.0),
                                    pal.secondary,
                                );
                            }
                        });
                    });
                ui.add_space(10.0);
                self.small_previews(ui, pal);
                if outline_button(ui, pal, "选择文件…").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter(
                            "图片与程序图标",
                            &[
                                "png", "jpg", "jpeg", "webp", "bmp", "ico", "exe", "dll", "icl",
                            ],
                        )
                        .pick_file()
                    {
                        self.choose_source_path(ctx, path);
                    }
                }
                if !self.studio_file.is_empty() {
                    let name = std::path::Path::new(&self.studio_file)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(name.as_ref())
                                .size(12.0)
                                .color(pal.secondary),
                        )
                        .truncate(),
                    )
                    .on_hover_text(&self.studio_file);
                }
                if ui.checkbox(&mut self.studio_design, "自定义编辑").changed() {
                    self.run_preview(ctx);
                }
                if self.studio_design {
                    self.style_controls(ui, ctx, pal);
                }
            });
        ui.add_space((ui.available_height() - 43.0).max(0.0));
        let ready = self.preview_master.is_some() && !self.mutation_busy();
        ui.horizontal(|ui| {
            let export_width = (ui.available_width() - 115.0).max(110.0);
            ui.add_enabled_ui(ready, |ui| {
                if primary_button(ui, pal, "导出图标…", export_width).clicked() {
                    self.export_studio(ctx);
                }
            });
            let directory_exists = self
                .studio_session
                .last_export
                .as_ref()
                .and_then(|p| p.parent())
                .is_some_and(|p| p.is_dir());
            ui.add_enabled_ui(directory_exists && !self.mutation_busy(), |ui| {
                if icon_button(ui, pal, Icon::FolderOpen, "打开位置", Some(104.0), false)
                    .on_hover_text(
                        self.studio_session
                            .last_export
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "导出后打开所在文件夹".into()),
                    )
                    .clicked()
                {
                    self.open_last_export(ctx);
                }
            });
        });
    }
    fn export_studio(&mut self, ctx: &egui::Context) {
        let Some(master) = self.preview_master.clone() else {
            return;
        };
        if self.mutation_busy() {
            return;
        }
        let stem = std::path::Path::new(&self.studio_file)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy();
        let mut dialog = rfd::FileDialog::new();
        if let Some(directory) = self
            .studio_session
            .export_directory
            .as_ref()
            .filter(|p| p.is_dir())
        {
            dialog = dialog.set_directory(directory);
        }
        let Some(mut output) = dialog
            .add_filter("Windows 图标", &["ico"])
            .add_filter("PNG 图片", &["png"])
            .set_file_name(format!("{stem}-设计.ico"))
            .save_file()
        else {
            return;
        };
        if output.extension().is_none() {
            output.set_extension("ico");
        }
        let source = PathBuf::from(&self.studio_file);
        let (tx, rx) = std::sync::mpsc::channel();
        self.export_rx = Some(rx);
        self.status = "正在导出图标…".into();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = crate::studio::export(&master, &output, &source)
                .map(|_| output)
                .map_err(|e| format!("导出失败：{e:#}"));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
    fn poll_export(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.export_rx.as_ref() else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
                return;
            }
            Err(_) => Err("导出任务中断，请重试".into()),
        };
        self.export_rx = None;
        match result {
            Ok(path) => {
                self.status = format!(
                    "已导出：{}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
                self.log_push(&format!("已导出：{}", path.display()));
                self.studio_session.export_directory = path.parent().map(|p| p.to_path_buf());
                self.studio_session.last_export = Some(path);
                self.session_dirty = Some(std::time::Instant::now());
            }
            Err(error) => {
                self.status = error;
                let message = self.status.clone();
                self.log_push(&message);
            }
        }
    }
}
impl App {
    fn toggle_exclusion(&mut self, path: &str) {
        let key = lnk::path_key(path);
        let mut excluded = self.excluded.clone();
        let removed = excluded.remove(&key);
        if !removed {
            excluded.insert(key);
        }
        let result = (|| -> anyhow::Result<()> {
            let mut cfg = AppConfig::load_checked()?;
            cfg.excluded_shortcuts = excluded.iter().cloned().collect();
            cfg.excluded_shortcuts.sort();
            cfg.save()
        })();
        match result {
            Ok(()) => {
                self.excluded = excluded;
                self.status = format!(
                    "{}：{}",
                    if removed {
                        "已取消排除"
                    } else {
                        "已排除"
                    },
                    display_lnk_name(path)
                );
            }
            Err(e) => self.status = format!("保存排除设置失败：{e:#}"),
        }
    }
    fn start_recovery(
        &mut self,
        ctx: &egui::Context,
        action: crate::recovery::Action,
        label: &str,
    ) {
        if self.mutation_busy() {
            return;
        }
        self.invalidate_reads();
        self.recovery_label = label.into();
        self.status = format!("{label}：正在准备…");
        self.recovery_progress = Some((0, 0));
        let (tx, rx) = std::sync::mpsc::channel();
        self.recovery_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = crate::recovery::run(action, |p| {
                let _ = tx.send(RecoveryEvent::Progress(p));
                ctx.request_repaint();
            })
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(RecoveryEvent::Finished(result));
            ctx.request_repaint();
        });
    }
    fn poll_recovery(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.recovery_rx.take() else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(RecoveryEvent::Progress(p)) => {
                    self.recovery_progress = Some((p.done, p.total));
                    self.status = format!(
                        "{}：{}/{} · {}",
                        self.recovery_label,
                        p.done,
                        p.total,
                        display_lnk_name(&p.name)
                    );
                }
                Ok(RecoveryEvent::Finished(result)) => {
                    self.recovery_progress = None;
                    let label = self.recovery_label.clone();
                    match result {
                        Ok(report) => {
                            let all_ok = report.results.iter().all(|r| r.ok);
                            self.after_restore(ctx, report.results, &label);
                            if report.skipped > 0 {
                                self.status
                                    .push_str(&format!(" · 跳过 {} 项", report.skipped));
                            }
                            if all_ok {
                                self.selected_snaps.clear();
                            }
                            self.applied = pipeline::applied_cache_load();
                            self.originals = pipeline::oldest_originals();
                            self.snapshots = pipeline::list_snapshots();
                        }
                        Err(error) => {
                            self.status = format!("{label}失败：{error}");
                            let message = self.status.clone();
                            self.log_push(&message);
                        }
                    }
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.recovery_rx = Some(rx);
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                    return;
                }
                Err(_) => {
                    self.recovery_progress = None;
                    self.status = "恢复任务中断，请查看日志后重试".into();
                    return;
                }
            }
        }
    }
}
fn checker_image(
    ui: &mut egui::Ui,
    texture: &TextureHandle,
    size: egui::Vec2,
    dark: bool,
    checkerboard: bool,
) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let a = if dark {
        egui::Color32::from_gray(54)
    } else {
        egui::Color32::from_gray(238)
    };
    if checkerboard {
        let b = if dark {
            egui::Color32::from_gray(67)
        } else {
            egui::Color32::from_gray(216)
        };
        let cell = 8.0;
        let columns = (size.x / cell).ceil() as usize;
        let rows = (size.y / cell).ceil() as usize;
        for row in 0..rows {
            for col in 0..columns {
                let tile = egui::Rect::from_min_size(
                    rect.min + egui::vec2(col as f32 * cell, row as f32 * cell),
                    egui::vec2(cell, cell),
                )
                .intersect(rect);
                ui.painter()
                    .rect_filled(tile, 0.0, if (row + col) % 2 == 0 { a } else { b });
            }
        }
    } else {
        ui.painter().rect_filled(rect, 0.0, a);
    }
    ui.painter().image(
        texture.id(),
        rect,
        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
}

impl App {
    fn editing_scope(&self) -> Option<String> {
        if self.active_page == 7 {
            Some("studio".into())
        } else if self.active_page == 0 && self.editing {
            self.selected_shortcut().map(|s| {
                format!(
                    "{}::{:?}::{:?}",
                    s.path,
                    self.tab,
                    if self.tab == SourceTab::Itunes {
                        self.cand_selected
                    } else {
                        None
                    }
                )
            })
        } else {
            None
        }
    }
    fn edit_state(&self) -> EditState {
        EditState {
            image: if self.active_page == 7 {
                self.studio_file.clone()
            } else if self.tab == SourceTab::File {
                self.file_path.clone()
            } else {
                String::new()
            },
            customize: if self.active_page == 7 {
                self.studio_design
            } else if self.tab == SourceTab::File {
                self.file_design
            } else {
                true
            },
            style: self.current_style("编辑"),
        }
    }
    fn ensure_history(&mut self) {
        let scope = self.editing_scope();
        if scope != self.history_scope {
            self.edit_history = History::new(self.edit_state());
            self.history_scope = scope;
        }
    }
    fn observe_editing(&mut self, ctx: &egui::Context) {
        self.ensure_history();
        if self.history_scope.is_some() {
            self.edit_history
                .observe(self.edit_state(), ctx.input(|i| i.pointer.primary_down()));
        }
    }
    fn apply_edit_state(&mut self, ctx: &egui::Context, state: &EditState) {
        self.base = state.style.base.clone();
        if let Some(color) = crate::gradient::parse_custom(&self.base) {
            self.custom_base = color;
        }
        self.glyph_scale = state.style.glyph_scale;
        self.icon_radius = state.style.icon_radius;
        self.base_size = state.style.base_size;
        self.base_radius = crate::composite::exponent_to_radius_pct(state.style.base_exponent);
        if self.active_page == 7 {
            self.studio_file = state.image.clone();
            self.studio_design = state.customize;
        } else if self.tab == SourceTab::File {
            self.file_path = state.image.clone();
            self.file_design = state.customize;
        }
        if self.active_page != 7 || !self.studio_file.is_empty() {
            self.run_preview(ctx);
        } else {
            self.invalidate_reads();
            self.after = None;
        }
    }
    fn undo_edit(&mut self, ctx: &egui::Context, redo: bool) {
        if self.mutation_busy() || self.history_scope.is_none() || self.draft_style.is_some() {
            return;
        }
        let state = if redo {
            self.edit_history.redo()
        } else {
            self.edit_history.undo()
        };
        if let Some(state) = state {
            self.apply_edit_state(ctx, &state);
        }
    }
    fn history_buttons(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        pal: &Palette,
        right_to_left: bool,
    ) {
        let available = !self.mutation_busy() && self.draft_style.is_none();
        for redo in if right_to_left {
            [true, false]
        } else {
            [false, true]
        } {
            let enabled = available
                && if redo {
                    self.edit_history.can_redo()
                } else {
                    self.edit_history.can_undo()
                };
            let clicked = ui
                .add_enabled_ui(enabled, |ui| {
                    icon_button(
                        ui,
                        pal,
                        if redo { Icon::Redo2 } else { Icon::Undo2 },
                        "",
                        Some(28.0),
                        false,
                    )
                    .on_hover_text(if redo {
                        "重做编辑 · Ctrl+Y / Ctrl+Shift+Z"
                    } else {
                        "撤销编辑 · Ctrl+Z"
                    })
                    .clicked()
                })
                .inner;
            if clicked {
                self.undo_edit(ctx, redo);
            }
        }
    }
    fn history_hotkeys(&mut self, ctx: &egui::Context) {
        if self.history_scope.is_none()
            || ctx
                .memory(|m| m.focused())
                .is_some_and(|id| egui::TextEdit::load_state(ctx, id).is_some())
            || self.mutation_busy()
            || self.draft_style.is_some()
        {
            return;
        }
        let shift = egui::Modifiers {
            ctrl: true,
            shift: true,
            ..Default::default()
        };
        let redo = ctx.input_mut(|i| {
            i.consume_key(shift, egui::Key::Z) || i.consume_key(egui::Modifiers::CTRL, egui::Key::Y)
        });
        if redo {
            self.undo_edit(ctx, true);
        } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Z)) {
            self.undo_edit(ctx, false);
        }
    }
    fn session_path() -> PathBuf {
        pipeline::data_dir().join("studio-session.json")
    }
    fn load_studio_session(&mut self, ctx: &egui::Context) {
        match StudioSession::load(&Self::session_path()) {
            Ok(Some(session)) => {
                let resume = session.resume_studio;
                self.studio_session = session;
                if resume {
                    self.start_studio(ctx);
                }
            }
            Ok(None) => {}
            Err(error) => {
                self.session_read_error = true;
                self.status = format!("读取制作进度失败：{error:#}；原文件已保留");
                let message = self.status.clone();
                self.log_push(&message);
            }
        }
    }
    fn sync_studio_session(&mut self, ctx: &egui::Context) {
        let active = self.active_page == 7;
        let mut changed = self.studio_session.resume_studio != active;
        if active {
            let state = self.edit_state();
            if !self.studio_session.state.equivalent(&state) {
                self.studio_session.state = state;
                changed = true;
            }
        }
        self.studio_session.resume_studio = active;
        if changed {
            self.session_dirty = Some(std::time::Instant::now());
        }
        if self.session_dirty.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(800));
        }
    }
    fn autosave_studio(&mut self, ctx: &egui::Context) {
        if self
            .session_dirty
            .is_some_and(|time| time.elapsed() >= std::time::Duration::from_millis(800))
            && !ctx.input(|i| i.pointer.primary_down())
        {
            self.flush_studio_session();
            ctx.request_repaint();
        }
    }
    fn flush_studio_session(&mut self) {
        if self.session_dirty.take().is_none() {
            return;
        }
        if self.session_read_error {
            self.status = "制作进度文件损坏，未覆盖原文件；本次仍可编辑和导出".into();
            return;
        }
        if let Err(error) = self.studio_session.save(&Self::session_path()) {
            self.status = format!("保存制作进度失败：{error:#}");
            let message = self.status.clone();
            self.log_push(&message);
        }
    }
    fn open_last_export(&mut self, ctx: &egui::Context) {
        if self.folder_rx.is_some() {
            return;
        }
        let Some(path) = self.studio_session.last_export.clone() else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.folder_rx = Some(rx);
        self.status = "正在打开导出位置…".into();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = crate::shell::open_export_location(&path)
                .map(|_| "已打开导出位置".into())
                .map_err(|e| format!("打开导出位置失败：{e:#}"));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
    fn status_feedback(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, pal: &Palette) {
        if self.status_seen != self.status {
            self.status_seen = self.status.clone();
            self.status_since = std::time::Instant::now();
        }
        let kind = crate::editing::feedback(&self.status);
        let expired = kind == Feedback::Success
            && self.status_since.elapsed() >= std::time::Duration::from_secs(5);
        if kind == Feedback::Success && !expired {
            ctx.request_repaint_after(
                std::time::Duration::from_secs(5).saturating_sub(self.status_since.elapsed()),
            );
        }
        let color = match kind {
            Feedback::Error => egui::Color32::from_rgb(207, 79, 74),
            Feedback::Success if !expired => {
                if self.dark_mode {
                    egui::Color32::from_rgb(128, 194, 149)
                } else {
                    egui::Color32::from_rgb(55, 132, 82)
                }
            }
            _ => pal.secondary,
        };
        if kind != Feedback::Info && !expired {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(7.0, 7.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 3.0, color);
        }
        ui.add(
            egui::Label::new(
                egui::RichText::new(if expired { "就绪" } else { &self.status })
                    .size(11.5)
                    .color(color),
            )
            .truncate(),
        )
        .on_hover_text(&self.status);
    }
}

impl App {
    fn small_previews(&self, ui: &mut egui::Ui, pal: &Palette) {
        if self.preview_master.is_none() || self.small_preview_textures.is_empty() {
            return;
        }
        ui.add_space(8.0);
        egui::CollapsingHeader::new("小尺寸预览 · 16 / 32 / 48")
            .id_salt(("small_previews", self.active_page))
            .show(ui, |ui| {
                egui::Frame::new()
                    .fill(pal.seg_bg)
                    .corner_radius(12.0)
                    .inner_margin(10)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            for (size, texture) in &self.small_preview_textures {
                                ui.allocate_ui_with_layout(
                                    egui::vec2(74.0, 72.0),
                                    egui::Layout::top_down(egui::Align::Center),
                                    |ui| {
                                        ui.allocate_ui_with_layout(
                                            egui::vec2(56.0, 50.0),
                                            egui::Layout::left_to_right(egui::Align::Center),
                                            |ui| {
                                                ui.add_space((56.0 - *size as f32) / 2.0);
                                                checker_image(
                                                    ui,
                                                    texture,
                                                    egui::Vec2::splat(*size as f32),
                                                    self.dark_mode,
                                                    self.show_checkerboard,
                                                );
                                            },
                                        );
                                        ui.label(
                                            egui::RichText::new(format!("{size} px"))
                                                .small()
                                                .color(pal.secondary),
                                        );
                                    },
                                );
                            }
                        });
                    });
            });
    }
    fn start_target_check(&mut self, ctx: &egui::Context) {
        if self.target_check_rx.is_some() {
            return;
        }
        let items = self.shortcuts.clone();
        let ctx = ctx.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.target_check_rx = Some(rx);
        self.status = "正在检测本地快捷方式目标…".into();
        std::thread::spawn(move || {
            let _ = tx.send(crate::maintenance::check_shortcuts(&items));
            ctx.request_repaint();
        });
    }
    fn refresh_backups(&mut self, ctx: &egui::Context, cleanup: Option<Vec<String>>) {
        if self.backup_rx.is_some() || self.mutation_busy() {
            return;
        }
        self.backup_deleting = cleanup.is_some();
        let (tx, rx) = std::sync::mpsc::channel();
        self.backup_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let root = pipeline::snapshots_dir();
            let message = cleanup.map(|ids| match crate::maintenance::cleanup(&root, &ids) {
                Ok(report) => {
                    let text = format!(
                        "已清理 {} 个备份，释放 {}",
                        report.removed,
                        format_bytes(report.bytes)
                    );
                    if report.errors.is_empty() {
                        text
                    } else {
                        format!("{text}；清理失败：{}", report.errors.join("；"))
                    }
                }
                Err(e) => format!("清理失败：{e:#}"),
            });
            let result = crate::maintenance::inventory(&root).map_err(|e| format!("{e:#}"));
            let _ = tx.send((result, message));
            ctx.request_repaint();
        });
    }
    fn poll_maintenance(&mut self, ctx: &egui::Context) {
        if let Some(rx) = self.target_check_rx.take() {
            match rx.try_recv() {
                Ok(checks) => {
                    let missing = checks
                        .values()
                        .filter(|h| matches!(h, crate::maintenance::TargetHealth::Missing))
                        .count();
                    let unknown = checks
                        .values()
                        .filter(|h| matches!(h, crate::maintenance::TargetHealth::Unknown(_)))
                        .count();
                    let skipped = checks
                        .values()
                        .filter(|h| matches!(h, crate::maintenance::TargetHealth::Skipped))
                        .count();
                    self.status = format!(
                        "检测完成：目标不存在 {missing} 项，待确认 {unknown} 项，跳过 {skipped} 项"
                    );
                    let message = self.status.clone();
                    self.log_push(&message);
                    self.target_checks = checks;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => self.target_check_rx = Some(rx),
                Err(_) => self.status = "快捷方式检测任务中断".into(),
            }
        }
        if let Some(rx) = self.backup_rx.take() {
            match rx.try_recv() {
                Ok((result, message)) => {
                    let deleted = self.backup_deleting;
                    self.backup_deleting = false;
                    if let Some(message) = message {
                        self.status = message.clone();
                        self.log_push(&message);
                    }
                    match result {
                        Ok(inventory) => {
                            self.backup_selected.retain(|id| {
                                inventory
                                    .rows
                                    .iter()
                                    .any(|r| &r.id == id && r.protected.is_none())
                            });
                            self.backup_inventory = Some(inventory);
                        }
                        Err(e) => {
                            self.backup_inventory = None;
                            self.status = format!("读取备份空间失败：{e}");
                        }
                    }
                    if deleted {
                        self.snapshots = pipeline::list_snapshots();
                        self.originals = pipeline::oldest_originals();
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => self.backup_rx = Some(rx),
                Err(_) => {
                    self.backup_deleting = false;
                    self.status = "备份管理任务中断，请刷新检查".into();
                }
            }
        }
        if self.target_check_rx.is_some() || self.backup_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
    fn backup_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, pal: &Palette) {
        ui.horizontal(|ui| {
            if icon_button(ui, pal, Icon::ArrowLeft, "返回", None, false).clicked() {
                self.active_page = 3;
            }
            ui.heading("备份空间");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(self.backup_rx.is_none(), egui::Button::new("刷新"))
                    .clicked()
                {
                    self.refresh_backups(ctx, None);
                }
            });
        });
        ui.add_space(12.0);
        if self.backup_rx.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(if self.backup_deleting {
                    "正在清理…"
                } else {
                    "正在统计…"
                });
            });
        }
        let mut requested = None;
        let busy = self.mutation_busy();
        if let Some(inventory) = &self.backup_inventory {
            ui.label(
                egui::RichText::new(format!(
                    "{} 个快照 · {}",
                    inventory.rows.len(),
                    format_bytes(inventory.bytes)
                ))
                .strong(),
            );
            ui.label(
                egui::RichText::new("保留最早原始记录和最新快照")
                    .small()
                    .color(pal.secondary),
            );
            ui.add_space(8.0);
            let chosen: Vec<String> = inventory
                .rows
                .iter()
                .filter(|r| self.backup_selected.contains(&r.id) && r.protected.is_none())
                .map(|r| r.id.clone())
                .collect();
            let bytes: u64 = inventory
                .rows
                .iter()
                .filter(|r| chosen.contains(&r.id))
                .map(|r| r.bytes)
                .sum();
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        self.backup_rx.is_none() && !busy,
                        egui::Button::new("选择可清理项"),
                    )
                    .clicked()
                {
                    self.backup_selected = inventory
                        .rows
                        .iter()
                        .filter(|r| r.protected.is_none())
                        .map(|r| r.id.clone())
                        .collect();
                }
                if ui
                    .add_enabled(
                        !chosen.is_empty() && self.backup_rx.is_none() && !busy,
                        egui::Button::new(format!("清理所选 · {}", format_bytes(bytes))),
                    )
                    .clicked()
                {
                    requested = Some(chosen);
                }
            });
            ui.add_space(8.0);
            egui::ScrollArea::vertical()
                .id_salt("backup_space")
                .show(ui, |ui| {
                    if inventory.rows.is_empty() {
                        ui.label("暂无快照备份");
                    }
                    for row in &inventory.rows {
                        egui::Frame::new()
                            .fill(pal.card)
                            .corner_radius(12.0)
                            .inner_margin(12)
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    let mut selected = self.backup_selected.contains(&row.id);
                                    if ui
                                        .add_enabled(
                                            row.protected.is_none() && self.backup_rx.is_none(),
                                            egui::Checkbox::new(&mut selected, ""),
                                        )
                                        .changed()
                                    {
                                        if selected {
                                            self.backup_selected.insert(row.id.clone());
                                        } else {
                                            self.backup_selected.remove(&row.id);
                                        }
                                    }
                                    ui.add(
                                        egui::Label::new(format!(
                                            "{} · {} 项",
                                            snapshot_time_label(&row.id),
                                            row.count
                                        ))
                                        .truncate(),
                                    )
                                    .on_hover_text(&row.id);
                                });
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(
                                        egui::RichText::new(format_bytes(row.bytes))
                                            .small()
                                            .color(pal.secondary),
                                    );
                                    if let Some(reason) = &row.protected {
                                        ui.label(
                                            egui::RichText::new(reason)
                                                .small()
                                                .color(pal.secondary),
                                        );
                                    }
                                });
                            });
                        ui.add_space(7.0);
                    }
                });
        }
        if let Some(ids) = requested {
            self.backup_confirm = Some(ids);
        }
        if let Some(ids) = self.backup_confirm.clone() {
            let mut confirm = false;
            let mut cancel = false;
            egui::Modal::new(egui::Id::unique("backup_cleanup_confirm")).show(ctx, |ui| {
                ui.set_width(300.0);
                ui.heading("清理所选备份");
                ui.add_space(10.0);
                let bytes = self.backup_inventory.as_ref().map_or(0, |inventory| {
                    inventory
                        .rows
                        .iter()
                        .filter(|r| ids.contains(&r.id))
                        .map(|r| r.bytes)
                        .sum()
                });
                ui.label(format!(
                    "将永久删除 {} 个快照备份，共 {}。",
                    ids.len(),
                    format_bytes(bytes)
                ));
                egui::ScrollArea::vertical()
                    .max_height(100.0)
                    .show(ui, |ui| {
                        for id in &ids {
                            ui.label(egui::RichText::new(id).small());
                        }
                    });
                ui.label("删除后无法从这些快照撤回。当前图标及快捷方式不会改动。");
                ui.horizontal(|ui| {
                    cancel = ui.button("取消").clicked();
                    confirm = ui
                        .add_enabled(
                            !self.mutation_busy() && self.backup_rx.is_none(),
                            egui::Button::new("确认清理"),
                        )
                        .clicked();
                });
            });
            if cancel {
                self.backup_confirm = None;
            }
            if confirm {
                self.backup_confirm = None;
                self.refresh_backups(ctx, Some(ids));
            }
        }
    }
}
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

impl App {
    fn gradient_controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(mut g) = crate::gradient::parse_linear(&self.base) else {
            return;
        };
        let mut changed = false;
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let mut a = [g.start.r, g.start.g, g.start.b];
            let mut b = [g.end.r, g.end.g, g.end.b];
            ui.label("起色");
            changed |= ui.color_edit_button_srgb(&mut a).changed();
            ui.label("终色");
            changed |= ui.color_edit_button_srgb(&mut b).changed();
            g.start = crate::gradient::Rgb::new(a[0], a[1], a[2]);
            g.end = crate::gradient::Rgb::new(b[0], b[1], b[2]);
            if ui.small_button("互换").clicked() {
                std::mem::swap(&mut g.start, &mut g.end);
                changed = true;
            }
        });
        ui.horizontal(|ui| {
            ui.label("方向");
            changed |= ui
                .add(egui::Slider::new(&mut g.angle, 0.0..=360.0).suffix("°"))
                .changed();
        });
        ui.horizontal_wrapped(|ui| {
            for (text, angle) in [("竖向", 0.0), ("横向", 90.0), ("斜向", 45.0)] {
                if ui.small_button(text).clicked() {
                    g.angle = angle;
                    changed = true;
                }
            }
        });
        self.gradient_draft = g;
        if changed {
            self.base = g.name();
            self.run_preview(ctx);
        }
    }
    fn choose_source_path(&mut self, ctx: &egui::Context, path: PathBuf) {
        let resource = path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|s| matches!(s.to_ascii_lowercase().as_str(), "exe" | "dll" | "icl"));
        if resource {
            self.start_resource_page(ctx, path, 0);
            return;
        }
        if self.active_page == 7 {
            self.studio_file = path.to_string_lossy().into_owned();
        } else {
            self.file_path = path.to_string_lossy().into_owned();
            self.tab = SourceTab::File;
        }
        self.run_preview(ctx);
    }
    fn pick_resource_file(&mut self, ctx: &egui::Context) {
        if self.asset_rx.is_some() {
            return;
        }
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("程序图标", &["exe", "dll", "icl", "ico"])
            .pick_file()
        {
            self.start_resource_page(ctx, path, 0);
        }
    }
    fn start_asset_job(
        &mut self,
        ctx: &egui::Context,
        writing: bool,
        job: impl FnOnce() -> anyhow::Result<AssetReply> + Send + 'static,
    ) {
        if self.asset_rx.is_some() || self.mutation_busy() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.asset_rx = Some(rx);
        self.asset_writing = writing;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job().map_err(|e| format!("{e:#}")));
            ctx.request_repaint();
        });
    }
    fn start_resource_page(&mut self, ctx: &egui::Context, file: PathBuf, start: u32) {
        if self.asset_rx.is_some() {
            return;
        }
        self.resource_picker = Some(ResourcePicker {
            file: file.clone(),
            total: 0,
            start,
            icons: Vec::new(),
        });
        self.status = "正在读取资源图标…".into();
        self.start_asset_job(ctx, false, move || {
            Ok(AssetReply::ResourcePage(crate::resources::page(
                &file, start,
            )?))
        });
    }
    fn poll_assets(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.asset_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.asset_writing = false;
                match result {
                    Ok(AssetReply::ResourcePage(page)) => {
                        if self
                            .resource_picker
                            .as_ref()
                            .is_some_and(|p| p.file == page.file)
                        {
                            let icons = page
                                .icons
                                .into_iter()
                                .filter_map(|(index, png)| {
                                    image::load_from_memory(&png).ok().map(|image| {
                                        (
                                            index,
                                            texture_from_image(
                                                ctx.clone(),
                                                &format!("resource-{index}"),
                                                &image.to_rgba8(),
                                            ),
                                        )
                                    })
                                })
                                .collect();
                            self.resource_picker = Some(ResourcePicker {
                                file: page.file,
                                total: page.total,
                                start: page.start,
                                icons,
                            });
                            self.status = format!("已读取资源，共 {} 个图标", page.total);
                        }
                    }
                    Ok(AssetReply::ResourceChosen(path)) => {
                        self.resource_picker = None;
                        self.choose_source_path(ctx, path);
                    }
                    Ok(AssetReply::ThemeLoaded(theme)) => {
                        self.theme_pending = Some(theme);
                        self.status = "主题套装已读取，等待确认导入".into();
                    }
                    Ok(AssetReply::ThemeInstalled(dir)) => {
                        self.style_presets = AppConfig::load().style_presets;
                        self.theme_pending = None;
                        self.prepare_icon_pack(ctx, &dir);
                    }
                    Ok(AssetReply::ThemeExported(path, count, skipped)) => {
                        self.status = format!(
                            "已导出主题：{}，{count} 个图标，跳过 {} 项",
                            path.file_name().unwrap_or_default().to_string_lossy(),
                            skipped.len()
                        );
                        let message = format!(
                            "{}；路径 {}；跳过：{}",
                            self.status,
                            path.display(),
                            skipped.join("、")
                        );
                        self.log_push(&message);
                    }
                    Err(e) => {
                        self.status = format!("操作失败：{e}");
                        let message = self.status.clone();
                        self.log_push(&message);
                    }
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => self.asset_rx = Some(rx),
            Err(_) => {
                self.asset_writing = false;
                self.status = "资源或主题任务中断，请重试".into();
            }
        }
        if self.asset_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
    fn resource_dialog(&mut self, ctx: &egui::Context, pal: &Palette) {
        let Some(picker) = &self.resource_picker else {
            return;
        };
        let file = picker.file.clone();
        let total = picker.total;
        let start = picker.start;
        let mut chosen = None;
        let mut page = None;
        let mut cancel = false;
        let mut browse = false;
        egui::Modal::new(egui::Id::unique("resource_picker")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("选择资源图标");
            ui.add(
                egui::Label::new(file.file_name().unwrap_or_default().to_string_lossy()).truncate(),
            )
            .on_hover_text(file.display().to_string());
            ui.add_space(8.0);
            if self.asset_rx.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("读取中…");
                });
            }
            if self.asset_rx.is_none() && picker.icons.is_empty() {
                ui.colored_label(pal.secondary, &self.status);
            }
            egui::ScrollArea::vertical()
                .max_height(310.0)
                .show(ui, |ui| {
                    egui::Grid::new("resource_grid")
                        .num_columns(6)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            for (i, (index, texture)) in picker.icons.iter().enumerate() {
                                ui.vertical(|ui| {
                                    let response = ui.add_enabled(
                                        self.asset_rx.is_none(),
                                        egui::Image::new((texture.id(), egui::vec2(44.0, 44.0)))
                                            .sense(egui::Sense::click()),
                                    );
                                    if response.hovered() {
                                        ui.painter().rect_stroke(
                                            response.rect.expand(2.0),
                                            6.0,
                                            egui::Stroke::new(1.5, ACCENT),
                                            egui::StrokeKind::Outside,
                                        );
                                    }
                                    if response.clicked() {
                                        chosen = Some(*index);
                                    }
                                    ui.label(
                                        egui::RichText::new(format!("#{index}"))
                                            .small()
                                            .color(pal.secondary),
                                    );
                                });
                                if (i + 1) % 6 == 0 {
                                    ui.end_row();
                                }
                            }
                        });
                });
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        start > 0 && self.asset_rx.is_none(),
                        egui::Button::new("上一页"),
                    )
                    .clicked()
                {
                    page = Some(start.saturating_sub(crate::resources::PAGE_SIZE));
                }
                ui.label(format!(
                    "{} / {}",
                    start / crate::resources::PAGE_SIZE + 1,
                    total.div_ceil(crate::resources::PAGE_SIZE).max(1)
                ));
                if ui
                    .add_enabled(
                        start + crate::resources::PAGE_SIZE < total && self.asset_rx.is_none(),
                        egui::Button::new("下一页"),
                    )
                    .clicked()
                {
                    page = Some(start + crate::resources::PAGE_SIZE);
                }
                if ui
                    .add_enabled(self.asset_rx.is_none(), egui::Button::new("换文件"))
                    .clicked()
                {
                    browse = true;
                }
                cancel = ui
                    .add_enabled(!self.asset_writing, egui::Button::new("取消"))
                    .clicked();
            });
        });
        if cancel {
            self.resource_picker = None;
        }
        if browse {
            self.pick_resource_file(ctx);
        }
        if let Some(start) = page {
            self.start_resource_page(ctx, file.clone(), start);
        }
        if let Some(index) = chosen {
            let uploads = pipeline::data_dir().join("uploads");
            self.start_asset_job(ctx, true, move || {
                Ok(AssetReply::ResourceChosen(crate::resources::select(
                    &file, index, &uploads,
                )?))
            });
        }
    }
    fn theme_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, pal: &Palette) {
        ui.horizontal(|ui| {
            if icon_button(ui, pal, Icon::ArrowLeft, "返回", None, false).clicked() {
                self.active_page = 3;
            }
            ui.heading("主题套装");
        });
        ui.add_space(12.0);
        egui::ScrollArea::vertical()
            .id_salt("theme_page")
            .show(ui, |ui| {
                egui::Frame::new()
                    .fill(pal.card)
                    .corner_radius(14.0)
                    .inner_margin(16)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(egui::RichText::new("导出当前图标").strong());
                        ui.add_space(8.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.theme_name)
                                .hint_text("主题名称")
                                .desired_width(ui.available_width()),
                        );
                        ui.label(format!(
                            "当前目录 {} 个快捷方式 · {} 个样式预设",
                            self.shortcuts.len(),
                            self.style_presets.len()
                        ));
                        ui.add_space(6.0);
                        if ui
                            .add_enabled(
                                self.asset_rx.is_none() && !self.shortcuts.is_empty(),
                                egui::Button::new("导出套装…"),
                            )
                            .clicked()
                        {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("IconForge 主题套装", &["iftheme"])
                                .set_file_name("我的主题.iftheme")
                                .save_file()
                            {
                                let name = self.theme_name.trim().to_owned();
                                let items = self.shortcuts.clone();
                                let mut presets = self.style_presets.clone();
                                let current = self.current_style("当前样式");
                                self.status = "正在打包主题…".into();
                                self.start_asset_job(ctx, true, move || {
                                    if !presets.iter().any(|p| p.same_style(&current)) {
                                        crate::theme_bundle::merge_presets(
                                            &mut presets,
                                            &[current],
                                        )?;
                                    }
                                    let (theme, skipped) =
                                        crate::theme_bundle::capture(name, items, presets)?;
                                    let count = theme.icons.len();
                                    crate::theme_bundle::write(&path, &theme)?;
                                    Ok(AssetReply::ThemeExported(path, count, skipped))
                                });
                            }
                        }
                    });
                ui.add_space(12.0);
                egui::Frame::new()
                    .fill(pal.card)
                    .corner_radius(14.0)
                    .inner_margin(16)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(egui::RichText::new("导入主题套装").strong());
                        ui.add_space(8.0);
                        if ui
                            .add_enabled(self.asset_rx.is_none(), egui::Button::new("选择套装…"))
                            .clicked()
                        {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("IconForge 主题套装", &["iftheme"])
                                .pick_file()
                            {
                                self.theme_pending = None;
                                self.status = "正在检查主题套装…".into();
                                self.start_asset_job(ctx, false, move || {
                                    Ok(AssetReply::ThemeLoaded(crate::theme_bundle::read(&path)?))
                                });
                            }
                        }
                        if let Some(theme) = self.theme_pending.clone() {
                            ui.add_space(8.0);
                            ui.label(egui::RichText::new(&theme.name).strong());
                            ui.label(format!(
                                "{} 个图标 · {} 个样式预设",
                                theme.icons.len(),
                                theme.presets.len()
                            ));
                            ui.checkbox(&mut self.theme_import_styles, "同时导入样式预设");
                            ui.label(
                                egui::RichText::new("导入后预览匹配，再选择应用")
                                    .small()
                                    .color(pal.secondary),
                            );
                            if ui
                                .add_enabled(
                                    self.asset_rx.is_none(),
                                    egui::Button::new("导入并预览匹配"),
                                )
                                .clicked()
                            {
                                let styles = self.theme_import_styles;
                                self.status = "正在导入主题套装…".into();
                                self.start_asset_job(ctx, true, move || {
                                    let mut config = AppConfig::load_checked()?;
                                    if styles {
                                        crate::theme_bundle::merge_presets(
                                            &mut config.style_presets,
                                            &theme.presets,
                                        )?;
                                    }
                                    let dir = crate::theme_bundle::install(
                                        &theme,
                                        &pipeline::data_dir().join("uploads/themes"),
                                    )?;
                                    if styles {
                                        config.save().map_err(|e| {
                                            anyhow::anyhow!(
                                                "图标已导入 {}，保存样式失败：{e:#}",
                                                dir.display()
                                            )
                                        })?;
                                    }
                                    Ok(AssetReply::ThemeInstalled(dir))
                                });
                            }
                        }
                    });
                if self.asset_rx.is_some() {
                    ui.add_space(8.0);
                    ui.spinner();
                }
            });
    }
}
