use chrono::Utc;

pub fn make_id(prefix: &str) -> String {
    format!(
        "{}-{}-{}",
        prefix,
        Utc::now().format("%Y%m%d-%H%M%S-%3f"),
        std::process::id()
    )
}
