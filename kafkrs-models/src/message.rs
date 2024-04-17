use chrono::serde::ts_nanoseconds;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Message<T> {
    key: String,
    value: T,
    #[serde(with = "ts_nanoseconds")]
    timestamp: DateTime<Utc>,
}
