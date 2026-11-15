//! Notes for the person or model running billy, outside the record stream.
//! A terminal gets them on stderr; the MCP server collects them so the
//! assistant sees them too, since it never sees stderr. Pipelines run on the
//! calling thread, so collecting is per thread.

use std::cell::RefCell;

thread_local! {
    static CAPTURED: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

pub fn note(message: impl Into<String>) {
    let message = message.into();
    CAPTURED.with(|c| match c.borrow_mut().as_mut() {
        Some(notes) => notes.push(message),
        None => eprintln!("{message}"),
    });
}

/// Runs `f`, returning its result and every note it made.
pub fn capture<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    let outer = CAPTURED.with(|c| c.replace(Some(Vec::new())));
    let result = f();
    let notes = CAPTURED.with(|c| c.replace(outer)).unwrap_or_default();
    (result, notes)
}
