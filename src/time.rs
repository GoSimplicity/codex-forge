use chrono::{DateTime, FixedOffset, Offset, Utc};

const BEIJING_OFFSET_SECONDS: i32 = 8 * 60 * 60;

fn beijing_offset() -> FixedOffset {
    FixedOffset::east_opt(BEIJING_OFFSET_SECONDS).unwrap_or_else(|| Utc.fix())
}

pub fn format_beijing(datetime: DateTime<Utc>, pattern: &str) -> String {
    datetime
        .with_timezone(&beijing_offset())
        .format(pattern)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::format_beijing;
    use chrono::{TimeZone, Utc};

    #[test]
    fn formats_utc_time_as_beijing_time() {
        let datetime = Utc.with_ymd_and_hms(2026, 3, 20, 0, 30, 0).unwrap();
        assert_eq!(
            format_beijing(datetime, "%Y-%m-%d %H:%M:%S"),
            "2026-03-20 08:30:00"
        );
    }
}
