use bincode::config;
use bincode::serde::encode_to_vec;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use kafkrs_models::message::Message;

#[pyfunction]
#[pyo3(signature = (key, value, schema))]
fn encode_message(
    py: Python,
    key: String,
    value: String,
    schema: Option<String>,
) -> PyResult<Bound<PyBytes>> {
    let message: Message<String> = Message {
        key,
        value,
        schema,
        timestamp: chrono::offset::Utc::now(),
    };
    let bin_conf = config::legacy();
    let bin_mess = encode_to_vec(&message, bin_conf).unwrap();
    let py_mess = PyBytes::new_bound(py, &bin_mess);
    Ok(py_mess)
}

#[pymodule]
fn kafkrs_python(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(encode_message, module)?)?;
    Ok(())
}
