//! Times for people: local time of day, as in the rest of their terminal.

use std::time::SystemTime;

/// `HH:MM:SS` in the system's time zone; UTC if that is unknown.
pub fn local_time(at: SystemTime) -> String {
    match jiff::Timestamp::try_from(at) {
        Ok(timestamp) => timestamp
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%H:%M:%S")
            .to_string(),
        Err(_) => "--:--:--".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_time_of_day() {
        let text = local_time(SystemTime::UNIX_EPOCH);
        assert_eq!(text.len(), 8);
        assert_eq!(&text[2..3], ":");
        assert!(text.ends_with(":00"), "{text}");
    }
}
