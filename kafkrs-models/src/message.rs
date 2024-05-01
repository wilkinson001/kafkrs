use chrono::serde::ts_nanoseconds;
use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(bound(serialize = "T: Serialize", deserialize = "T: DeserializeOwned"))]
pub struct Message<T> {
    pub key: String,
    pub value: T,
    #[serde(with = "ts_nanoseconds")]
    pub timestamp: DateTime<Utc>,
}
