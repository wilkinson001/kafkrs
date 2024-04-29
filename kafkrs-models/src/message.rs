use chrono::{DateTime, Utc};
use chrono::serde::ts_nanoseconds;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(bound(serialize = "T: Serialize", deserialize = "T: DeserializeOwned"))]
pub struct Message<T> {
    key: String,
    value: T,
    #[serde(with = "ts_nanoseconds")]
    timestamp: DateTime<Utc>,
}
