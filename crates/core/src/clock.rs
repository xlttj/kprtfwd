//! Times for people: in the system's time zone, as in the rest of their
//! terminal (UTC if that is unknown), and with the date unless it is clear.
//! The daemon log, the TUI and `kprtfwd logs` all use it, so their times
//! agree.

use std::time::SystemTime;

use jiff::Timestamp;
use jiff::tz::TimeZone;

/// For the TUI's event view, where width is short: `HH:MM:SS` for a time of
/// today, else `YYYY-MM-DD HH:MM:SS`. Events live as long as the daemon,
/// which may be days.
pub fn local_time(at: SystemTime) -> String {
    time_in(at, SystemTime::now(), &TimeZone::system())
}

/// For `kprtfwd logs`: `YYYY-MM-DD HH:MM:SS`.
pub fn local_date_time(at: SystemTime) -> String {
    date_time_in(at, &TimeZone::system())
}

/// For the daemon's log: now, in RFC 3339 with microseconds and the
/// offset, e.g. `2026-11-07T09:15:02.123456+01:00`.
pub fn log_timestamp() -> String {
    timestamp_in(Timestamp::now(), &TimeZone::system())
}

fn time_in(at: SystemTime, now: SystemTime, zone: &TimeZone) -> String {
    let Ok(at) = Timestamp::try_from(at) else {
        return "--:--:--".into();
    };
    let at = at.to_zoned(zone.clone());
    let today =
        Timestamp::try_from(now).is_ok_and(|now| now.to_zoned(zone.clone()).date() == at.date());
    let format = if today {
        "%H:%M:%S"
    } else {
        "%Y-%m-%d %H:%M:%S"
    };
    at.strftime(format).to_string()
}

fn date_time_in(at: SystemTime, zone: &TimeZone) -> String {
    match Timestamp::try_from(at) {
        Ok(at) => at
            .to_zoned(zone.clone())
            .strftime("%Y-%m-%d %H:%M:%S")
            .to_string(),
        Err(_) => "---------- --:--:--".into(),
    }
}

fn timestamp_in(at: Timestamp, zone: &TimeZone) -> String {
    at.to_zoned(zone.clone())
        .strftime("%Y-%m-%dT%H:%M:%S%.6f%:z")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> SystemTime {
        text.parse::<Timestamp>().unwrap().into()
    }

    fn plus_one() -> TimeZone {
        TimeZone::fixed(jiff::tz::offset(1))
    }

    #[test]
    fn the_date_only_when_not_today() {
        let now = at("2026-11-07T12:00:00Z");
        assert_eq!(
            time_in(at("2026-11-07T08:15:02Z"), now, &plus_one()),
            "09:15:02"
        );
        // Yesterday in UTC, but today in the zone.
        assert_eq!(
            time_in(at("2026-11-06T23:30:00Z"), now, &plus_one()),
            "00:30:00"
        );
        assert_eq!(
            time_in(at("2026-11-06T22:59:59Z"), now, &plus_one()),
            "2026-11-06 23:59:59"
        );
        assert_eq!(
            time_in(SystemTime::UNIX_EPOCH, now, &TimeZone::UTC),
            "1970-01-01 00:00:00"
        );
    }

    #[test]
    fn date_and_time_in_the_zone() {
        assert_eq!(
            date_time_in(at("2026-11-06T23:30:00Z"), &plus_one()),
            "2026-11-07 00:30:00"
        );
    }

    #[test]
    fn the_log_has_the_offset() {
        let at = "2026-11-07T08:15:02.123456789Z".parse().unwrap();
        assert_eq!(
            timestamp_in(at, &plus_one()),
            "2026-11-07T09:15:02.123456+01:00"
        );
        assert_eq!(
            timestamp_in(at, &TimeZone::UTC),
            "2026-11-07T08:15:02.123456+00:00"
        );
    }

    #[test]
    fn in_the_system_zone() {
        let now = SystemTime::now();
        let date_time = local_date_time(now);
        assert_eq!(date_time.len(), "YYYY-MM-DD HH:MM:SS".len());
        assert!(date_time.ends_with(&local_time(now)), "{date_time}");
        assert_eq!(
            log_timestamp().len(),
            "2026-11-07T09:15:02.123456+01:00".len()
        );
    }
}
