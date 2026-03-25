use chrono::Utc;
use std::sync::atomic::{AtomicU64, Ordering};

static ID_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn make_id(prefix: &str) -> String {
    format!(
        "{}-{}-{}-{}",
        prefix,
        Utc::now().format("%Y%m%d-%H%M%S-%3f"),
        std::process::id(),
        ID_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}
