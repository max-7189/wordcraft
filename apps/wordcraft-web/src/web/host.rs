//! Embedding bridge: a page that shows WordCraft in an `<iframe>` hands it a document and takes the
//! saved bytes back, instead of the file picker and a download.
//!
//! Off unless the page is framed and loaded with `?embed=<origin>`, where `<origin>` is the exact
//! origin of the embedding page (`https://example.com`). Messages (`window.postMessage`) are
//! accepted only from `window.parent` at exactly that origin, and sent only to it.
//!
//! Host → WordCraft:
//! - `{type: "open", name, bytes}` (`bytes`: an `ArrayBuffer` or `Uint8Array`): opens the document
//!   as if it had been picked, replacing the current one without asking (the host decides when).
//! - `{type: "font", name, bytes, document?}`: a font file (`.ttf`/`.otf`/…) for the interface — a
//!   fallback after the embedded faces, so a Chinese or Japanese interface doesn't draw boxes (the
//!   web build embeds no CJK face and can't read system fonts) — and, unless `document: false`,
//!   for documents too, added as File › Options › Add Font does and kept in the browser. The
//!   interface needs it on every visit; documents get it back from the browser's storage.
//! - `{type: "save"}`: File › Save.
//! - `{type: "save-failed"}`: the host could not keep the last save; the document counts as
//!   unsaved again.
//!
//! WordCraft → host:
//! - `{type: "ready"}` once the app runs (send `open` after it);
//! - `{type: "dirty", dirty}` whenever "has unsaved changes" flips;
//! - `{type: "saved", name, bytes}` on every Save of a Word document (Ctrl+S, the ribbon, or a
//!   `save` request), with the `ArrayBuffer` transferred. Exports and saves in other formats still
//!   download.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

/// Largest document a host may hand over (as a picked file).
const MAX_OPEN_BYTES: u32 = 200 << 20;

/// What the host asked for, handled by the shell on its next frame.
pub enum Request {
    Open { name: String, bytes: Vec<u8> },
    Font { name: String, bytes: Vec<u8>, document: bool },
    Save,
    SaveFailed,
}

pub struct Host {
    origin: String,
    requests: RefCell<Vec<Request>>,
    dirty: Cell<Option<bool>>,
}

fn parent_window() -> Option<web_sys::Window> {
    let window = web_sys::window()?;
    let parent = window.parent().ok().flatten()?;
    // A top-level page is its own parent: nothing embeds it.
    if js_sys::Object::is(&parent, &window) { None } else { Some(parent) }
}

fn text(data: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok().and_then(|v| v.as_string())
}

impl Host {
    /// The bridge, when this page is framed and was loaded with `?embed=<origin>`.
    pub fn from_location() -> Option<Rc<Host>> {
        let search = web_sys::window()?.location().search().ok()?;
        let origin = web_sys::UrlSearchParams::new_with_str(&search).ok()?.get("embed")?;
        // A wildcard would hand the document to whoever frames the page.
        if origin.is_empty() || origin == "*" || origin == "null" {
            log::error!("embed: an exact origin is required (?embed=https://example.com)");
            return None;
        }
        parent_window()?;
        Some(Rc::new(Host { origin, requests: RefCell::new(Vec::new()), dirty: Cell::new(None) }))
    }

    /// Start accepting the host's messages; each one wakes the app.
    pub fn listen(self: &Rc<Self>, ctx: egui::Context) -> Result<(), String> {
        let window = web_sys::window().ok_or("no window")?;
        let host = Rc::clone(self);
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
            if e.origin() != host.origin {
                return;
            }
            let from_parent = match (e.source(), parent_window()) {
                (Some(source), Some(parent)) => js_sys::Object::is(&source, &parent),
                _ => false,
            };
            if !from_parent {
                return;
            }
            let data = e.data();
            let kind = text(&data, "type");
            let request = match kind.as_deref() {
                Some(kind @ ("open" | "font")) => {
                    let bytes = js_sys::Reflect::get(&data, &JsValue::from_str("bytes")).unwrap_or(JsValue::UNDEFINED);
                    if !(bytes.is_instance_of::<js_sys::ArrayBuffer>() || bytes.is_instance_of::<js_sys::Uint8Array>()) {
                        log::error!("embed: `{kind}` needs `bytes` (ArrayBuffer or Uint8Array)");
                        return;
                    }
                    let bytes = js_sys::Uint8Array::new(&bytes);
                    if bytes.length() == 0 || bytes.length() > MAX_OPEN_BYTES {
                        log::error!("embed: `{kind}` bytes are empty or larger than 200 MB");
                        return;
                    }
                    let name = text(&data, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "document.docx".into());
                    if kind == "open" {
                        Request::Open { name, bytes: bytes.to_vec() }
                    } else if wordcraft_fonts::is_font_file_name(&name.to_ascii_lowercase()) {
                        let document = js_sys::Reflect::get(&data, &JsValue::from_str("document")).ok().and_then(|v| v.as_bool()).unwrap_or(true);
                        Request::Font { name, bytes: bytes.to_vec(), document }
                    } else {
                        log::error!("embed: `font` needs a font file name (.ttf, .otf, …)");
                        return;
                    }
                }
                Some("save") => Request::Save,
                Some("save-failed") => Request::SaveFailed,
                _ => return,
            };
            host.requests.borrow_mut().push(request);
            ctx.request_repaint();
        });
        window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).map_err(|e| format!("{e:?}"))?;
        // The listener lives as long as the page.
        on_message.forget();
        Ok(())
    }

    /// The host's requests since the last frame, oldest first.
    pub fn take_requests(&self) -> Vec<Request> {
        std::mem::take(&mut *self.requests.borrow_mut())
    }

    pub fn ready(&self) {
        self.post(&[("type", JsValue::from_str("ready"))], None);
    }

    /// Called after every pass; tells the host only when the flag changes.
    pub fn dirty(&self, dirty: bool) {
        if self.dirty.replace(Some(dirty)) != Some(dirty) {
            self.post(&[("type", JsValue::from_str("dirty")), ("dirty", JsValue::from_bool(dirty))], None);
        }
    }

    /// Whether a save named `name` goes to the host (a Word document) rather than a download.
    pub fn keeps(name: &str) -> bool {
        std::path::Path::new(name).extension().and_then(|e| e.to_str()).is_some_and(|e| wordcraft_engine::io::is_word_package(&e.to_ascii_lowercase()))
    }

    /// Hand a saved document to the host.
    pub fn saved(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let array = js_sys::Uint8Array::from(bytes);
        let buffer = array.buffer();
        let transfer = js_sys::Array::of1(&buffer);
        if self.post(&[("type", JsValue::from_str("saved")), ("name", JsValue::from_str(name)), ("bytes", buffer.into())], Some(&transfer)) {
            Ok(())
        } else {
            Err("the embedding page could not be reached".into())
        }
    }

    fn post(&self, fields: &[(&str, JsValue)], transfer: Option<&js_sys::Array>) -> bool {
        let Some(parent) = parent_window() else { return false };
        let message = js_sys::Object::new();
        for (key, value) in fields {
            let _ = js_sys::Reflect::set(&message, &JsValue::from_str(key), value);
        }
        let sent = match transfer {
            Some(transfer) => parent.post_message_with_transfer(&message, &self.origin, transfer),
            None => parent.post_message(&message, &self.origin),
        };
        if let Err(e) = &sent {
            log::error!("embed: postMessage failed: {e:?}");
        }
        sent.is_ok()
    }
}
