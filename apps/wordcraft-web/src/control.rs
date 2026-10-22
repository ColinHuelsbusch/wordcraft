//! `request(method, params)`: the control channel of the desktop app (`docs/control-protocol.md`)
//! for the page, as a promise. It is what `wordcraft-cli mcp --connect` drives over TCP; in the browser an
//! agent (WebMCP) or a test calls the same methods on the wasm module's export, the shape
//! EffectCraft's `request` and FilmCraft's `window.filmcraft.request` have.
//!
//! ```js
//! const commands = await bindings.request("engine.commands");
//! await bindings.request("engine.execute", { command: commands[0].id, params: {} });
//! ```
//!
//! A result resolves with the method's `result`; an error rejects with an `Error` carrying its
//! message. The app answers in its next frame (a screenshot a frame later), so the answers are
//! polled on a short timer while any is outstanding, and the app is asked to repaint meanwhile.

use std::cell::{Cell, RefCell};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use serde_json::{Value, json};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::*;
use wordcraft_ui_egui::ControlRequest;

type Pending = (Receiver<Value>, js_sys::Function, js_sys::Function);

thread_local! {
    static TX: RefCell<Option<Sender<ControlRequest>>> = const { RefCell::new(None) };
    static CTX: RefCell<Option<egui::Context>> = const { RefCell::new(None) };
    static PENDING: RefCell<Vec<Pending>> = const { RefCell::new(Vec::new()) };
    static POLLING: Cell<bool> = const { Cell::new(false) };
}

/// How often the outstanding answers are looked for, in milliseconds.
const POLL_MS: i32 = 10;

/// The receiving end for `with_control`; `request` sends into it and wakes `ctx`.
pub fn channel(ctx: &egui::Context) -> Receiver<ControlRequest> {
    let (tx, rx) = std::sync::mpsc::channel();
    TX.with(|t| *t.borrow_mut() = Some(tx));
    CTX.with(|c| *c.borrow_mut() = Some(ctx.clone()));
    rx
}

/// Send one control request; the promise settles when the app has answered it.
#[wasm_bindgen]
pub fn request(method: String, params: JsValue) -> js_sys::Promise {
    let params = from_js(&params);
    js_sys::Promise::new(&mut |resolve, reject| {
        let (req, rx) = ControlRequest::new(method.clone(), params.clone());
        let sent = TX.with(|t| t.borrow().as_ref().is_some_and(|tx| tx.send(req).is_ok()));
        if !sent {
            let _ = reject.call1(&JsValue::NULL, &js_sys::Error::new("WordCraft is not running yet"));
            return;
        }
        PENDING.with(|p| p.borrow_mut().push((rx, resolve, reject)));
        wake();
    })
}

fn to_js(v: &Value) -> JsValue {
    js_sys::JSON::parse(&v.to_string()).unwrap_or(JsValue::NULL)
}

fn from_js(v: &JsValue) -> Value {
    if v.is_undefined() || v.is_null() {
        return json!({});
    }
    js_sys::JSON::stringify(v).ok().and_then(|s| s.as_string()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}))
}

fn repaint() {
    CTX.with(|c| {
        if let Some(ctx) = c.borrow().as_ref() {
            ctx.request_repaint();
        }
    });
}

/// Ask for a frame and, unless it already runs, start looking for the answers.
fn wake() {
    repaint();
    if !POLLING.with(|p| p.replace(true)) {
        schedule();
    }
}

fn schedule() {
    let tick = wasm_bindgen::closure::Closure::once_into_js(poll);
    let set = web_sys::window().is_some_and(|w| w.set_timeout_with_callback_and_timeout_and_arguments_0(tick.unchecked_ref(), POLL_MS).is_ok());
    if !set {
        POLLING.with(|p| p.set(false));
    }
}

/// Settle the promises whose answers arrived; keep looking while any is outstanding.
fn poll() {
    let mut settled = Vec::new();
    let left = PENDING.with(|p| {
        let mut p = p.borrow_mut();
        let mut keep = Vec::with_capacity(p.len());
        for (rx, resolve, reject) in p.drain(..) {
            match rx.try_recv() {
                Ok(v) => settled.push((Some(v), resolve, reject)),
                Err(TryRecvError::Empty) => keep.push((rx, resolve, reject)),
                Err(TryRecvError::Disconnected) => settled.push((None, resolve, reject)),
            }
        }
        *p = keep;
        p.len()
    });
    // Outside the borrow: settling runs no script now, but a later `request` must find the list free.
    for (answer, resolve, reject) in settled {
        match answer {
            Some(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                let _ = resolve.call1(&JsValue::NULL, &to_js(v.get("result").unwrap_or(&Value::Null)));
            }
            Some(v) => {
                let msg = v.get("error").and_then(Value::as_str).map_or_else(|| v.to_string(), str::to_string);
                let _ = reject.call1(&JsValue::NULL, &js_sys::Error::new(&msg));
            }
            None => {
                let _ = reject.call1(&JsValue::NULL, &js_sys::Error::new("request dropped"));
            }
        }
    }
    if left == 0 {
        POLLING.with(|p| p.set(false));
    } else {
        repaint();
        schedule();
    }
}
