use arrow_array::builder::BinaryBuilder;
use arrow_array::{
    ArrayRef, BinaryArray, Int32Array, Int64Array, RecordBatch, TimestampNanosecondArray,
};
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The WAL/wire envelope. `offset` is assigned by the PartitionWriter at
/// group-commit time; producers send the rest. Empty key == "no key" (v1
/// collapses null and empty).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Record {
    pub offset: i64,
    pub timestamp_ns: i64,
    pub schema_id: u32,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

/// v1 envelope-only Parquet schema (spec §"Parquet schema (v1, envelope-only)").
/// Column order is load-bearing: `records_to_recordbatch` builds arrays in this
/// order and the segment writer relies on it.
pub fn parquet_arrow_schema() -> Schema {
    Schema::new(vec![
        Field::new("offset", DataType::Int64, false),
        Field::new(
            "timestamp_ns",
            DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
            false,
        ),
        Field::new("key", DataType::Binary, true),
        Field::new("value", DataType::Binary, false),
        Field::new("schema_id", DataType::Int32, false),
    ])
}

pub fn records_to_recordbatch(records: &[Record]) -> RecordBatch {
    let offsets: Int64Array =
        Int64Array::from(records.iter().map(|r| r.offset).collect::<Vec<_>>());
    let timestamps: TimestampNanosecondArray =
        TimestampNanosecondArray::from(records.iter().map(|r| r.timestamp_ns).collect::<Vec<_>>())
            .with_timezone("UTC");

    let mut key_builder: BinaryBuilder = BinaryBuilder::new();
    for r in records {
        if r.key.is_empty() {
            key_builder.append_null();
        } else {
            key_builder.append_value(&r.key);
        }
    }
    let keys: BinaryArray = key_builder.finish();

    let mut value_builder: BinaryBuilder = BinaryBuilder::new();
    for r in records {
        value_builder.append_value(&r.value);
    }
    let values: BinaryArray = value_builder.finish();

    let schema_ids: Int32Array = Int32Array::from(
        records
            .iter()
            .map(|r| r.schema_id as i32)
            .collect::<Vec<_>>(),
    );

    RecordBatch::try_new(
        Arc::new(parquet_arrow_schema()),
        vec![
            Arc::new(offsets) as ArrayRef,
            Arc::new(timestamps),
            Arc::new(keys),
            Arc::new(values),
            Arc::new(schema_ids),
        ],
    )
    .expect("record arrays must match parquet_arrow_schema")
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::Array;

    fn sample(offset: i64) -> Record {
        Record {
            offset,
            timestamp_ns: 1_700_000_000_000_000_000 + offset,
            schema_id: 0,
            key: vec![1, 2, 3],
            value: vec![9, 9, 9, 9],
        }
    }

    #[test]
    fn parquet_schema_has_envelope_columns_in_order() {
        let schema = parquet_arrow_schema();
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(
            names,
            vec!["offset", "timestamp_ns", "key", "value", "schema_id"]
        );
        assert!(!schema.field(0).is_nullable()); // offset REQUIRED
        assert!(schema.field(2).is_nullable()); // key OPTIONAL
        assert!(!schema.field(3).is_nullable()); // value REQUIRED
    }

    #[test]
    fn records_to_recordbatch_roundtrips_values() {
        let recs = vec![sample(0), sample(1)];
        let rb = records_to_recordbatch(&recs);
        assert_eq!(rb.num_rows(), 2);
        assert_eq!(rb.num_columns(), 5);
    }

    #[test]
    fn null_key_is_represented_when_empty() {
        let mut r = sample(0);
        r.key = vec![];
        let rb = records_to_recordbatch(&[r]);
        let keys = rb
            .column(2)
            .as_any()
            .downcast_ref::<arrow_array::BinaryArray>()
            .unwrap();
        assert!(keys.is_null(0));
    }
}
