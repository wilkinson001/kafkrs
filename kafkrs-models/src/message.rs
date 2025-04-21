use chrono::serde::ts_nanoseconds;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Message {
    pub key: String,
    pub value: Vec<u8>,
    pub schema: Option<String>,
    pub partition: Option<String>,
    #[serde(with = "ts_nanoseconds")]
    pub timestamp: DateTime<Utc>,
}
