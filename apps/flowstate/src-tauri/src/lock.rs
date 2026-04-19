// Mutex lock helper.
//
// Phase 4.7 of the architecture audit: poisoning was handled
// inconsistently across src-tauri (some modules used
// `.lock().unwrap()`, some recovered via `poisoned.into_inner()`). A
// poisoned mutex here is never fatal — we're guarding maps of child
// handles and in-flight task tokens, all of which tolerate seeing a
// mid-update snapshot. Standardise on recovery so one panic inside a
// held lock can't wedge the whole daemon.

use std::sync::{Mutex, MutexGuard};

/// Take a lock, returning the guard even if the mutex has been
/// poisoned by a previous panic. Call at every `.lock()` site that
/// used to `.unwrap()`.
pub fn lock_ok<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}
