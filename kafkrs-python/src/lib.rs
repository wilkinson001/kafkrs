use bincode::config;
use bincode::serde::encode_to_vec;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use kafkrs_models::record::Record;

#[pyfunction]
#[pyo3(signature = (key, value, schema_id, timestamp_ns=0))]
fn encode_message<'py>(
    py: Python<'py>,
    key: Vec<u8>,
    value: Vec<u8>,
    schema_id: u32,
    timestamp_ns: i64,
) -> PyResult<Bound<'py, PyBytes>> {
    let ts: i64 = if timestamp_ns != 0 {
        timestamp_ns
    } else {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64
    };
    let record: Record = Record {
        offset: 0, // assigned by the broker at commit time
        timestamp_ns: ts,
        schema_id,
        key,
        value,
    };
    let bin: Vec<u8> = encode_to_vec(&record, config::standard()).unwrap();
    Ok(PyBytes::new_bound(py, &bin))
}

#[pymodule]
fn kafkrs_python(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(encode_message, module)?)?;
    Ok(())
}
