//! A Ctrl+C flag without a crate: install a `SIGINT` handler that sets an
//! atomic. The C runtime's `signal` exists on every target std supports.

use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, Ordering};

static STOP: AtomicBool = AtomicBool::new(false);

const SIGINT: c_int = 2;

extern "C" {
    fn signal(sig: c_int, handler: usize) -> usize;
}

extern "C" fn on_signal(_: c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Install the handler. Idempotent.
pub fn install() {
    // SAFETY: `on_signal` only touches an atomic, which is async-signal-safe.
    unsafe {
        signal(SIGINT, on_signal as *const () as usize);
    }
}

pub fn requested() -> bool {
    STOP.load(Ordering::SeqCst)
}
