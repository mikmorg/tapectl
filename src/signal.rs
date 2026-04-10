use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Install the SIGINT handler. Call once at startup.
pub fn install_handler() {
    ctrlc::set_handler(|| {
        INTERRUPTED.store(true, Ordering::SeqCst);
    })
    .expect("failed to install SIGINT handler");
}

/// Check whether Ctrl+C has been pressed.
pub fn is_interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Reset the interrupted flag (e.g., after handling an interruption).
pub fn clear_interrupted() {
    INTERRUPTED.store(false, Ordering::SeqCst);
}
