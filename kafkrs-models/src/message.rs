use arrow_array::builder::BinaryBuilder;
use arrow_array::{ArrayRef, RecordBatch, StringArray, TimestampNanosecondArray};
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use chrono::serde::ts_nanoseconds;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
pub struct Message {
    pub key: String,
    pub value: Vec<u8>,
    pub schema: Option<String>,
    pub partition: Option<String>,
    #[serde(with = "ts_nanoseconds")]
    pub timestamp: DateTime<Utc>,
}

pub fn arrow_schema() -> Schema {
    let key: Field = Field::new("key", DataType::Utf8, false);
    let value: Field = Field::new("value", DataType::Binary, false);
    let schema: Field = Field::new("schema", DataType::Utf8, true);
    let partition: Field = Field::new("partition", DataType::Utf8, true);
    let timestamp: Field = Field::new(
        "partition",
        DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
        false,
    );

    Schema::new(vec![key, value, schema, partition, timestamp])
}

pub fn messages_to_recordbatch(messages: &Vec<Message>) -> RecordBatch {
    let keys = StringArray::from(messages.iter().map(|m| m.key.clone()).collect::<Vec<_>>());
    let mut value_builder = BinaryBuilder::new();
    for m in messages {
        value_builder.append_value(&m.value);
    }
    let values = value_builder.finish();
    let schemas = StringArray::from(
        messages
            .iter()
            .map(|m| m.schema.clone())
            .collect::<Vec<_>>(),
    );
    let partitions = StringArray::from(
        messages
            .iter()
            .map(|m| m.partition.clone())
            .collect::<Vec<_>>(),
    );
    let timestamps = TimestampNanosecondArray::from(
        messages
            .iter()
            .map(|m| m.timestamp.timestamp_nanos_opt())
            .collect::<Vec<_>>(),
    );

    match RecordBatch::try_new(
        Arc::new(arrow_schema()),
        vec![
            Arc::new(keys) as ArrayRef,
            Arc::new(values),
            Arc::new(schemas),
            Arc::new(partitions),
            Arc::new(timestamps),
        ],
    ) {
        Ok(rb) => rb,
        Err(e) => panic!("Could not create Recordbatch: {e:?}"),
    }
}
