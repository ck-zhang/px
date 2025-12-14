use std::env;

use time::OffsetDateTime;

fn sandbox_timestamp() -> OffsetDateTime {
    if let Ok(raw) = env::var("SOURCE_DATE_EPOCH") {
        if let Ok(epoch) = raw.trim().parse::<i64>() {
            if let Ok(ts) = OffsetDateTime::from_unix_timestamp(epoch) {
                return ts;
            }
        }
    }
    OffsetDateTime::UNIX_EPOCH
}

pub(crate) fn sandbox_timestamp_string() -> String {
    sandbox_timestamp()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
