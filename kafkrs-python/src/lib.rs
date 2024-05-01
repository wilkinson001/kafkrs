use bincode::config;
use bincode::serde::encode_to_vec;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use kafkrs_models::message::Message;

/// Encodes a key/value to a binary representation of a Message
#[pyfunction]
fn encode_message(py: Python, key: String, value: String) -> PyResult<&PyBytes> {
    let message: Message<String> = Message {
        key,
        value,
        timestamp: chrono::offset::Utc::now(),
    };
    let bin_conf = config::legacy();
    let bin_mess = encode_to_vec(&message, bin_conf).unwrap();
    let py_mess = PyBytes::new(py, &bin_mess);
    Ok(py_mess)
}

/// A Python module implemented in Rust.
#[pymodule]
fn kafkrs_python(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(encode_message, m)?)?;
    Ok(())
}
