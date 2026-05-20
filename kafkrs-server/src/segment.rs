use anyhow::Result;
use arrow_array::RecordBatch;
use bytes::Bytes;
use kafkrs_models::record::{records_to_recordbatch, Record};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::schema::types::ColumnPath;

/// Serializes records into a single-row-group Parquet object per the spec's
/// writer settings.
pub fn write_segment(records: &[Record]) -> Result<Bytes> {
    let batch: RecordBatch = records_to_recordbatch(records);
    let props: WriterProperties = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?))
        .set_max_row_group_size(usize::MAX) // 1 row group per segment
        .set_data_page_size_limit(1024 * 1024) // 1 MiB pages
        .set_statistics_enabled(EnabledStatistics::Page) // page indexes
        .set_dictionary_enabled(false)
        .set_column_dictionary_enabled(ColumnPath::from("schema_id"), true)
        .build();
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut w: ArrowWriter<&mut Vec<u8>> =
            ArrowWriter::try_new(&mut buf, batch.schema(), Some(props))?;
        w.write(&batch)?;
        w.close()?;
    }
    Ok(Bytes::from(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    fn rec(o: i64) -> Record {
        Record {
            offset: o,
            timestamp_ns: 1_700_000_000_000_000_000 + o,
            schema_id: 5,
            key: vec![1],
            value: vec![2, 3],
        }
    }

    #[test]
    fn roundtrips_through_parquet_single_row_group() {
        let recs: Vec<Record> = (0..1000).map(rec).collect();
        let bytes = write_segment(&recs).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(bytes.clone()).unwrap();
        let meta = reader.metadata().clone();
        assert_eq!(meta.num_row_groups(), 1);
        let mut r = reader.build().unwrap();
        let batch = r.next().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 1000);
    }
}
