//! The browser shell: web `Services`, drag-and-drop, the unsaved-changes guard and the eframe
//! web runner.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wordcraft_engine::Session;
use wordcraft_ui_egui::{Inbox, Services, WordApp};

const DOC_EXTS: &[&str] = &["docx", "docm", "dotx", "dotm", "doc", "dot", "odt", "rtf", "txt", "md", "html", "htm", "tex", "json", "xml"];
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];
const RECIPIENT_EXTS: &[&str] = &["csv", "tsv", "txt", "xlsx", "xlsm", "ods"];
mod font_store;
mod host;
const CANVAS_ID: &str = "wordcraft_canvas";
const LOADING_ID: &str = "wordcraft_loading";

pub fn start() {
    eframe::WebLogger::init(log::LevelFilter::Info).ok();
    wasm_bindgen_futures::spawn_local(async {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            log::error!("no document");
            return;
        };
        let Some(canvas) = document.get_element_by_id(CANVAS_ID).and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok()) else {
            log::error!("missing <canvas id=\"{CANVAS_ID}\">");
            return;
        };
        let dirty = Rc::new(Cell::new(false));
        let saves = Rc::new(PickerSaves::default());
        // Embedded (`?embed=<origin>`): the host page owns opening, saving and unsaved changes.
        let host = host::Host::from_location();
        if host.is_none()
            && let Err(e) = guard_unload(dirty.clone(), saves.clone())
        {
            log::error!("no unsaved-changes guard: {e}");
        }
        let mut options = eframe::WebOptions::default();
        if query().contains("webgl")
            && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup
        {
            create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
        }
        let failed_host = host.clone();
        let result = eframe::WebRunner::new()
            .start(
                canvas,
                options,
                Box::new(move |cc| {
                    if let Some(rs) = &cc.wgpu_render_state {
                        log::info!("wordcraft-web: wgpu backend {:?}", rs.adapter.get_info().backend);
                    }
                    // Fonts the user added on an earlier visit (#582); the app notices them arrive.
                    font_store::load_all(cc.egui_ctx.clone());
                    let inbox: Inbox = Inbox::default();
                    if let Err(e) = listen_for_pasted_pictures(inbox.clone(), cc.egui_ctx.clone()) {
                        log::error!("pasting pictures is unavailable: {e}");
                    }
                    // The browser's locale picks the new document's paper, the units and the
                    // decimal separator (#838).
                    let region = wordcraft_ui_egui::i18n::system_region();
                    let sample = query().contains("sample");
                    let doc = if sample { wordcraft_engine::sample::sample_document() } else { wordcraft_engine::blank_document(&region) };
                    let mut session = Session::new(doc);
                    session.system_region = region;
                    let mut app = WordApp::new(session, services(inbox.clone(), cc.egui_ctx.clone(), dirty, saves.clone(), host.clone()));
                    app.autosave = false;
                    // No document to show: start on File › Home, as the desktop app does (#766).
                    // Embedded, the host sends the document.
                    if !sample && host.is_none() {
                        app.start_screen();
                    }
                    if let Some(host) = &host {
                        app.show_title_bar = false;
                        app.single_document = true;
                        app.ribbon_layout = host.ribbon;
                        if let Some(lang) = host.lang.as_deref().and_then(wordcraft_ui_egui::i18n::lang_from_tag)
                            && let Err(e) = app.run("ui.language", serde_json::json!({"value": lang.code()}))
                        {
                            log::error!("embed: can't set the interface language: {e}");
                        }
                        match host.listen(cc.egui_ctx.clone()) {
                            Ok(()) => host.ready(),
                            Err(e) => log::error!("embed: can't listen to the host: {e}"),
                        }
                    }
                    Ok(Box::new(WebShell { app, inbox, saves, host, ui_fonts: Vec::new(), awaiting_open: None, quiet_font: false }))
                }),
            )
            .await;
        if let Some(el) = document.get_element_by_id(LOADING_ID) {
            match result {
                Ok(()) => el.remove(),
                Err(e) => {
                    el.set_inner_html(&format!("<p>WordCraft failed to start: {e:?}</p><p>A browser with WebGPU or WebGL2 is required.</p>"));
                    if let Some(host) = &failed_host {
                        host.failed(&format!("{e:?}"));
                    }
                }
            }
        }
    });
}

fn query() -> String {
    web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default()
}

/// Wraps the app to read dropped files asynchronously (browsers can't read them synchronously)
/// and feed them through the inbox.
struct WebShell {
    app: WordApp,
    inbox: Inbox,
    saves: Rc<PickerSaves>,
    host: Option<Rc<host::Host>>,
    /// Interface fallback faces from the host (name, bytes), kept installed ([`WebShell::keep_ui_fonts`]).
    ui_fonts: Vec<(String, &'static [u8])>,
    /// The document id before a host `open`, until another document has opened ([`host::Host::opened`]).
    awaiting_open: Option<u64>,
    /// A host font went to the inbox this pass: its "Added …" status message is not shown.
    quiet_font: bool,
}

/// Saves the browser's save picker finishes after `download` has returned (#728). The app marks
/// the document saved as soon as `download` returns, so a cancelled or failed picker save must
/// mark it unsaved again, and the `beforeunload` guard keeps warning while one is in flight.
#[derive(Default)]
struct PickerSaves {
    /// Picker saves started and not yet written and closed.
    pending: Cell<u32>,
    /// Picker saves that didn't write the file: (name, error, or `None` when cancelled).
    unsaved: RefCell<Vec<(String, Option<String>)>>,
}

impl WebShell {
    /// The app reinstalls the interface fonts now and then (language, input method), which drops
    /// faces added from outside; adding one that is installed is a no-op, so this runs every pass.
    fn keep_ui_fonts(&self, ctx: &egui::Context) {
        for (name, bytes) in &self.ui_fonts {
            let families = [egui::FontFamily::Proportional, egui::FontFamily::Name("medium".into()), egui::FontFamily::Name("semibold".into())]
                .into_iter()
                .map(|family| egui::epaint::text::InsertFontFamily { family, priority: egui::epaint::text::FontPriority::Lowest })
                .collect();
            ctx.add_font(egui::epaint::text::FontInsert::new(name, egui::FontData::from_static(bytes), families));
        }
    }

    /// The embedding page's requests ([`host`]).
    fn serve_host(&mut self) {
        let Some(host) = self.host.clone() else { return };
        for request in host.take_requests() {
            match request {
                host::Request::Open { name, bytes } => {
                    // The host replaces the document on purpose (a newer version): don't ask to save.
                    self.app.session.dirty = false;
                    self.awaiting_open = Some(self.app.session.document_id());
                    self.inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
                }
                host::Request::Font { name, bytes, document } => {
                    if document {
                        // The inbox adds a font file as Add Font does (and keeps it in the browser).
                        self.inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name.clone(), bytes.clone()));
                        self.quiet_font = true;
                    }
                    // Lives as long as the page: egui keeps a reference to every installed face.
                    let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
                    self.ui_fonts.push((format!("host {name}"), bytes));
                }
                host::Request::Save => {
                    if let Err(e) = self.app.run("file.save", serde_json::json!({})) {
                        log::error!("embed: save failed: {e}");
                    }
                }
                host::Request::SaveFailed => self.app.session.dirty = true,
                host::Request::Ribbon(layout) => self.app.ribbon_layout = layout,
            }
        }
    }

    /// A picker save that didn't happen leaves the document with unsaved changes.
    fn settle_picker_saves(&mut self) {
        let unsaved = std::mem::take(&mut *self.saves.unsaved.borrow_mut());
        for (name, error) in unsaved {
            // Only a save in a format that keeps everything marked the document saved.
            if wordcraft_ui_egui::keeps_everything(&name) {
                self.app.session.dirty = true;
            }
            if let Some(error) = error {
                let msg = wordcraft_ui_egui::i18n::fmt(
                    wordcraft_ui_egui::i18n::t("Couldn't download {name}: {error}"),
                    &[("name", &name), ("error", &error)],
                );
                self.app.status(msg);
            }
        }
    }
}

impl eframe::App for WebShell {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let dropped = ctx.input_mut(|i| std::mem::take(&mut i.raw.dropped_files));
        for f in dropped {
            // Embedded, a dropped document must not replace the one the host gave (its Save would
            // check the other document in as a new version of the host's file); pictures still insert.
            let lower = f.path().to_string_lossy().to_ascii_lowercase();
            if self.host.is_some() && !IMAGE_EXTS.iter().any(|e| lower.ends_with(&format!(".{e}"))) && !wordcraft_fonts::is_font_file_name(&lower) {
                log::warn!("embed: dropped file ignored: {lower}");
                continue;
            }
            let inbox = self.inbox.clone();
            let ctx = ctx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let name = f.path().file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "dropped".into());
                match f.bytes_async().await {
                    Ok(bytes) => {
                        inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
                        ctx.request_repaint();
                    }
                    Err(e) => log::error!("couldn't read dropped file {name}: {e}"),
                }
            });
        }
        self.settle_picker_saves();
        self.serve_host();
        self.keep_ui_fonts(ctx);
        self.app.logic(ctx);
        if let Some(host) = &self.host {
            let document = self.app.session.document_id();
            host.document.set(document);
            if self.awaiting_open.is_some_and(|before| before != document) {
                self.awaiting_open = None;
                host.opened(document);
            }
            if std::mem::take(&mut self.quiet_font) {
                self.app.status_msg = None;
            }
        }
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.app.raw_input_hook(raw);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
    }
}

/// Largest picture pasted from the browser's clipboard (as `insert.picture` allows).
const MAX_PASTED_BYTES: f64 = (200u32 << 20) as f64;
/// Most pictures one paste inserts.
const MAX_PASTED_PICTURES: u32 = 20;

/// Pasting a picture (#45): eframe's paste handler only reads text, so a clipboard holding just
/// pictures (a screenshot, a browser's Copy Image) is read here and the pictures arrive through
/// the inbox like dropped ones. A clipboard with text is left to eframe.
fn listen_for_pasted_pictures(inbox: Inbox, ctx: egui::Context) -> Result<(), String> {
    let document = web_sys::window().and_then(|w| w.document()).ok_or("no document")?;
    let on_paste = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::ClipboardEvent)>::new(move |e: web_sys::ClipboardEvent| {
        let Some(data) = e.clipboard_data() else { return };
        if !data.get_data("text").unwrap_or_default().is_empty() {
            return;
        }
        let Some(files) = data.files() else { return };
        for i in 0..files.length().min(MAX_PASTED_PICTURES) {
            let Some(file) = files.get(i) else { continue };
            if !file.type_().starts_with("image/") {
                continue;
            }
            if file.size() > MAX_PASTED_BYTES {
                log::warn!("pasted picture is larger than 200 MB; not inserted");
                continue;
            }
            let inbox = inbox.clone();
            let ctx = ctx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await {
                    Ok(buf) => {
                        let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                        // The inbox inserts files named like pictures; the engine reads the format from the bytes.
                        inbox.lock().unwrap_or_else(|e| e.into_inner()).push(("pasted.png".into(), bytes));
                        ctx.request_repaint();
                    }
                    Err(e) => log::error!("couldn't read the pasted picture: {e:?}"),
                }
            });
        }
    });
    document.add_event_listener_with_callback("paste", on_paste.as_ref().unchecked_ref()).map_err(|e| format!("{e:?}"))?;
    // The listener lives as long as the page.
    on_paste.forget();
    Ok(())
}

/// `dirty` is the flag the `beforeunload` guard reads ([`guard_unload`]).
fn services(inbox: Inbox, ctx: egui::Context, dirty: Rc<Cell<bool>>, saves: Rc<PickerSaves>, host: Option<Rc<host::Host>>) -> Services {
    let open_inbox = inbox.clone();
    let save_ctx = ctx.clone();
    let save_host = host.clone();
    let embedded = host.is_some();
    Services {
        open_async: Some(Box::new(move |purpose: &str| {
            // Embedded, the host owns the document: File › Open (Ctrl+O) has nothing to pick.
            if embedded && !matches!(purpose, "picture" | "font" | "recipients") {
                log::warn!("embed: opening another document is not available here");
                return;
            }
            let inbox = open_inbox.clone();
            let ctx = ctx.clone();
            let dialog = if purpose == "picture" {
                rfd::AsyncFileDialog::new().add_filter("Pictures", IMAGE_EXTS)
            } else if purpose == "font" {
                rfd::AsyncFileDialog::new().add_filter("Fonts", wordcraft_fonts::FONT_FILE_EXTS)
            } else if purpose == "recipients" {
                rfd::AsyncFileDialog::new().add_filter("Recipient lists", RECIPIENT_EXTS)
            } else {
                rfd::AsyncFileDialog::new().add_filter("Documents", DOC_EXTS)
            };
            wasm_bindgen_futures::spawn_local(async move {
                let Some(file) = dialog.pick_file().await else {
                    return;
                };
                let bytes = file.read().await;
                inbox.lock().unwrap_or_else(|e| e.into_inner()).push((file.file_name(), bytes));
                ctx.request_repaint();
            });
        })),
        // Exports ask for a name; the browser decides where the download goes.
        pick_save: Some(Box::new(|name: &str| Some(name.to_string()))),
        download: Some(Box::new(move |path: &str, bytes: &[u8]| match &save_host {
            // Embedded: a Word document goes back to the host page; exports still download.
            Some(host) if host::Host::keeps(path) => host.saved(path, bytes),
            _ => download(&saves, &save_ctx, path, bytes),
        })),
        print: Some(Box::new(print_pdf)),
        inbox: Some(inbox),
        on_dirty: Some(Box::new(move |d| {
            dirty.set(d);
            if let Some(host) = &host {
                host.dirty(d);
            }
        })),
        animations_enabled: Some(animations_enabled()),
        store_font: Some(Box::new(font_store::store)),
        forget_font: Some(Box::new(font_store::forget)),
        ..Default::default()
    }
}

/// Whether the system has animation effects on, as the browser passes it on: off while the
/// `prefers-reduced-motion: reduce` media query matches. `None` when the browser can't say. The
/// query is made once and asked every frame; it follows the system's setting by itself.
fn animations_enabled() -> Box<dyn Fn() -> Option<bool>> {
    let reduce = web_sys::window().and_then(|w| w.match_media("(prefers-reduced-motion: reduce)").ok().flatten());
    Box::new(move || reduce.as_ref().map(|query| !query.matches()))
}

/// Closing, reloading or leaving the tab with unsaved changes asks first. Browsers show their own
/// confirmation (a page can only ask for it), so there is no Save button: Cancel, then File › Save.
fn guard_unload(dirty: Rc<Cell<bool>>, saves: Rc<PickerSaves>) -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    let on_unload = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::BeforeUnloadEvent)>::new(move |e: web_sys::BeforeUnloadEvent| {
        // A save still in the browser's picker hasn't written anything yet.
        if dirty.get() || saves.pending.get() > 0 {
            e.prevent_default();
            // Older browsers ask only when `returnValue` is set.
            e.set_return_value("unsaved changes");
        }
    });
    window.add_event_listener_with_callback("beforeunload", on_unload.as_ref().unchecked_ref()).map_err(|e| format!("{e:?}"))?;
    // The listener lives as long as the page.
    on_unload.forget();
    Ok(())
}

/// Trigger a browser save dialog or download of `bytes` named after the last component of `path`.
fn download(saves: &Rc<PickerSaves>, ctx: &egui::Context, path: &str, bytes: &[u8]) -> Result<(), String> {
    let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "document".into());
    let window = web_sys::window().ok_or("no window")?;

    // On browsers supporting the File System Access API (Chromium: Chrome, Edge, etc.),
    // `showSaveFilePicker` lets the user choose the save location and filename natively.
    if let Ok(picker_val) = js_sys::Reflect::get(&window, &wasm_bindgen::JsValue::from_str("showSaveFilePicker"))
        && let Ok(picker_fn) = picker_val.dyn_into::<js_sys::Function>()
    {
        match trigger_save_file_picker(saves, ctx, &window, &picker_fn, &name, bytes) {
            Ok(()) => return Ok(()),
            Err(e) => {
                log::warn!("showSaveFilePicker failed: {e}; falling back to anchor download");
            }
        }
    }

    // Fallback for browsers without showSaveFilePicker (e.g. Firefox, Safari):
    trigger_anchor_download(&window, &name, bytes)
}

fn trigger_save_file_picker(
    saves: &Rc<PickerSaves>,
    ctx: &egui::Context,
    window: &web_sys::Window,
    picker_fn: &js_sys::Function,
    name: &str,
    bytes: &[u8],
) -> Result<(), String> {
    let js = |e: wasm_bindgen::JsValue| format!("{e:?}");
    let ext = std::path::Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("docx").to_ascii_lowercase();
    let dot_ext = format!(".{ext}");
    let mime = mime_for(name);

    let accept_obj = js_sys::Object::new();
    let exts_arr = js_sys::Array::new();
    exts_arr.push(&wasm_bindgen::JsValue::from_str(&dot_ext));
    let _ = js_sys::Reflect::set(&accept_obj, &wasm_bindgen::JsValue::from_str(mime), &exts_arr);

    // Our own neutral description: the format's name, as its extension says it.
    let type_desc = match ext.as_str() {
        "htm" => "HTML".to_string(),
        "txt" => "Text".to_string(),
        e if !e.is_empty() && e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric()) => e.to_ascii_uppercase(),
        _ => "Document".to_string(),
    };

    let type_obj = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&type_obj, &wasm_bindgen::JsValue::from_str("description"), &wasm_bindgen::JsValue::from_str(&type_desc));
    let _ = js_sys::Reflect::set(&type_obj, &wasm_bindgen::JsValue::from_str("accept"), &accept_obj);

    let types_arr = js_sys::Array::new();
    types_arr.push(&type_obj);

    let picker_opts = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&picker_opts, &wasm_bindgen::JsValue::from_str("suggestedName"), &wasm_bindgen::JsValue::from_str(name));
    let _ = js_sys::Reflect::set(&picker_opts, &wasm_bindgen::JsValue::from_str("types"), &types_arr);

    // Call showSaveFilePicker synchronously while transient user activation is active:
    let prom_val = picker_fn.call1(window, &picker_opts).map_err(js)?;
    let prom: js_sys::Promise = prom_val.dyn_into().map_err(|_| "not a promise".to_string())?;

    let bytes_vec = bytes.to_vec();
    let saves = saves.clone();
    let ctx = ctx.clone();
    let name = name.to_string();
    saves.pending.set(saves.pending.get().saturating_add(1));
    wasm_bindgen_futures::spawn_local(async move {
        // The document counts as saved only once the file is written and closed.
        let unsaved = match wasm_bindgen_futures::JsFuture::from(prom).await {
            Ok(handle_val) => match write_file_handle(&handle_val, &bytes_vec).await {
                Ok(()) => {
                    log::info!("saved {name} through the save picker");
                    None
                }
                Err(e) => {
                    log::error!("couldn't write {name}: {e}");
                    Some(Some(e))
                }
            },
            Err(e) => {
                let err_name =
                    js_sys::Reflect::get(&e, &wasm_bindgen::JsValue::from_str("name")).ok().and_then(|v| v.as_string()).unwrap_or_default();
                if err_name == "AbortError" {
                    log::debug!("save picker cancelled");
                    Some(None)
                } else {
                    log::error!("showSaveFilePicker error: {e:?}");
                    Some(Some(format!("{e:?}")))
                }
            }
        };
        if let Some(error) = unsaved {
            saves.unsaved.borrow_mut().push((name, error));
        }
        saves.pending.set(saves.pending.get().saturating_sub(1));
        ctx.request_repaint();
    });
    Ok(())
}

async fn write_file_handle(handle: &wasm_bindgen::JsValue, bytes: &[u8]) -> Result<(), String> {
    let js = |e: wasm_bindgen::JsValue| format!("{e:?}");
    let create_fn: js_sys::Function = js_sys::Reflect::get(handle, &wasm_bindgen::JsValue::from_str("createWritable"))
        .map_err(js)?
        .dyn_into()
        .map_err(|_| "createWritable is not a function".to_string())?;
    let stream_prom: js_sys::Promise = create_fn.call0(handle).map_err(js)?.dyn_into().map_err(|_| "not a promise".to_string())?;
    let stream = wasm_bindgen_futures::JsFuture::from(stream_prom).await.map_err(js)?;

    let write_fn: js_sys::Function = js_sys::Reflect::get(&stream, &wasm_bindgen::JsValue::from_str("write"))
        .map_err(js)?
        .dyn_into()
        .map_err(|_| "write is not a function".to_string())?;
    let uint8 = js_sys::Uint8Array::from(bytes);
    let write_prom: js_sys::Promise = write_fn.call1(&stream, &uint8).map_err(js)?.dyn_into().map_err(|_| "not a promise".to_string())?;
    wasm_bindgen_futures::JsFuture::from(write_prom).await.map_err(js)?;

    let close_fn: js_sys::Function = js_sys::Reflect::get(&stream, &wasm_bindgen::JsValue::from_str("close"))
        .map_err(js)?
        .dyn_into()
        .map_err(|_| "close is not a function".to_string())?;
    let close_prom: js_sys::Promise = close_fn.call0(&stream).map_err(js)?.dyn_into().map_err(|_| "not a promise".to_string())?;
    wasm_bindgen_futures::JsFuture::from(close_prom).await.map_err(js)?;
    Ok(())
}

fn trigger_anchor_download(window: &web_sys::Window, name: &str, bytes: &[u8]) -> Result<(), String> {
    let js = |e: wasm_bindgen::JsValue| format!("{e:?}");
    let document = window.document().ok_or("no document")?;
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type(mime_for(name));
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).map_err(js)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(js)?;
    let a: web_sys::HtmlAnchorElement = document.create_element("a").map_err(js)?.dyn_into().map_err(|_| "not an anchor".to_string())?;
    a.set_href(&url);
    a.set_download(name);
    a.style().set_property("display", "none").map_err(js)?;
    let body = document.body().ok_or("no body")?;
    body.append_child(&a).map_err(js)?;
    a.click();
    a.remove();
    // Revoke after the click has been dispatched; the download keeps its own reference.
    let revoke = wasm_bindgen::closure::Closure::once_into_js(move || {
        web_sys::Url::revoke_object_url(&url).ok();
    });
    window.set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), 10_000).map_err(js)?;
    Ok(())
}

/// The hidden iframe the last print loaded its PDF into.
const PRINT_FRAME_ID: &str = "wordcraft-print-frame";

/// Opens `bytes` (a PDF) as an object URL in a hidden iframe and calls the
/// iframe's own `print()` once it has finished loading — this is the
/// standard way to drive the browser's native print dialog on a PDF without
/// a download or a popup window the browser might block. The previous
/// print's iframe and object URL are removed first, so only one copy of the
/// document's PDF stays in the page (the last one may still be printing).
fn print_pdf(bytes: &[u8]) -> Result<(), String> {
    let js = |e: wasm_bindgen::JsValue| format!("{e:?}");
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;

    if let Some(old) = document.get_element_by_id(PRINT_FRAME_ID) {
        if let Ok(frame) = old.clone().dyn_into::<web_sys::HtmlIFrameElement>() {
            let src = frame.src();
            if src.starts_with("blob:") {
                web_sys::Url::revoke_object_url(&src).ok();
            }
        }
        old.remove();
    }

    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type("application/pdf");
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).map_err(js)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(js)?;

    let iframe: web_sys::HtmlIFrameElement =
        match document.create_element("iframe").map_err(js).and_then(|e| e.dyn_into().map_err(|_| "not an iframe".to_string())) {
            Ok(f) => f,
            Err(e) => {
                web_sys::Url::revoke_object_url(&url).ok();
                return Err(e);
            }
        };
    iframe.set_id(PRINT_FRAME_ID);
    let attached = iframe
        .style()
        .set_property("display", "none")
        .map_err(js)
        .and_then(|()| document.body().ok_or_else(|| "no body".to_string()))
        .and_then(|body| body.append_child(&iframe).map_err(js));
    if let Err(e) = attached {
        iframe.remove();
        web_sys::Url::revoke_object_url(&url).ok();
        return Err(e);
    }

    // `onload` fires once the PDF has actually rendered inside the iframe;
    // printing before that would show a blank page. Set the handler BEFORE
    // `src` so a fast/cached load can't fire before we're listening.
    let iframe_for_load = iframe.clone();
    let on_load = wasm_bindgen::closure::Closure::once_into_js(move || {
        if let Some(w) = iframe_for_load.content_window() {
            // Ignore errors: some browsers (notably Firefox with a built-in
            // PDF viewer) print fine but report a benign cross-origin-style
            // error back on this call.
            let _ = w.print();
        }
    });
    iframe.set_onload(Some(on_load.unchecked_ref()));
    iframe.set_src(&url);
    Ok(())
}

fn mime_for(name: &str) -> &'static str {
    match name.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("pdf") => "application/pdf",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("odt") => "application/vnd.oasis.opendocument.text",
        Some("rtf") => "application/rtf",
        Some("html") => "text/html",
        Some("md" | "txt") => "text/plain",
        Some("tex") => "application/x-tex",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        _ => "application/octet-stream",
    }
}
