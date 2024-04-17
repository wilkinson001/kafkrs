use chrono::{DateTime, Utc};
use chrono::serde::ts_nanoseconds;
use serde::{Serialize, Deserialize};


#[derive(Serialize, Deserialize)]
struct Message<T> {
    key: String,
    value: T,
    #[serde(with = "ts_nanoseconds")]
    timestamp: DateTime<Utc>
}
