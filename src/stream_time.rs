use crate::streams::Vod;

// This is display metadata only. Replay coverage and alignment keep the original
// absolute timestamps, including when a missing/invalid start falls back to end.
pub(crate) fn recording_time(vod: &Vod) -> Option<time::OffsetDateTime> {
    let parse = |text: &str| {
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()
    };
    vod.started_at
        .as_deref()
        .and_then(parse)
        .or_else(|| vod.ended_at.as_deref().and_then(parse))
}

pub(crate) fn recording_local_time(vod: &Vod) -> Option<(time::OffsetDateTime, &'static str)> {
    recording_time(vod).and_then(central_european)
}

// Use the current EU rules (since 2002): last Sundays of March/October at 01:00 UTC.
// https://eur-lex.europa.eu/eli/dir/2000/84/oj
pub(crate) fn pull_start_time(start_ms: i64) -> Option<(time::OffsetDateTime, &'static str)> {
    let utc =
        time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(start_ms) * 1_000_000).ok()?;
    central_european(utc)
}

pub(crate) fn central_european(
    at: time::OffsetDateTime,
) -> Option<(time::OffsetDateTime, &'static str)> {
    let utc = at.checked_to_offset(time::UtcOffset::UTC)?;
    if utc.year() < 2002 {
        return None;
    }
    let transition = |month| {
        let last = time::Date::from_calendar_date(utc.year(), month, 31).ok()?;
        let sunday = 31 - last.weekday().number_days_from_sunday();
        Some(
            time::Date::from_calendar_date(utc.year(), month, sunday)
                .ok()?
                .with_hms(1, 0, 0)
                .ok()?
                .assume_utc(),
        )
    };
    let summer = utc >= transition(time::Month::March)? && utc < transition(time::Month::October)?;
    let (hours, zone) = if summer { (2, "CEST") } else { (1, "CET") };
    let local = utc.checked_to_offset(time::UtcOffset::from_hms(hours, 0, 0).ok()?)?;
    Some((local, zone))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_end_fallback_never_fabricates_replay_start_or_coverage() {
        let mut vod: Vod = serde_json::from_value(serde_json::json!({
            "userId":"1", "name":"Player", "provider":"twitch", "url":"https://www.twitch.tv/videos/1",
            "startedAt":"invalid", "endedAt":"2026-10-01T11:06:00Z"
        })).unwrap();
        for start in [Some("invalid"), Some(""), None] {
            vod.started_at = start.map(str::to_owned);
            let (at, zone) = recording_local_time(&vod).unwrap();
            assert_eq!((at.hour(), at.minute(), zone), (13, 6, "CEST"));
            assert!(vod.as_stream().replay_start_ms.is_none());
            assert!(vod.as_stream().replay_range().is_none());
        }
        vod.ended_at = Some("2026-02-30T11:06:00Z".into());
        assert!(recording_local_time(&vod).is_none());
    }
}
