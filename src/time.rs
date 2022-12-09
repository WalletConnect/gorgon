use chrono::{NaiveDateTime, Utc};

const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

#[deprecated(since = "0.1.2", note = "please use `now` amd `format` instead")]
pub fn create_timestamp() -> String {
    Utc::now().format(TIMESTAMP_FORMAT).to_string()
}

pub fn now() -> NaiveDateTime {
    let now = Utc::now();
    NaiveDateTime::from_timestamp_opt(now.timestamp(), now.timestamp_subsec_nanos())
        .expect("invalid timestamp")
}

pub fn format(t: &NaiveDateTime) -> String {
    t.format(TIMESTAMP_FORMAT).to_string()
}
