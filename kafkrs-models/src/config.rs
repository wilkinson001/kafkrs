use serde::Deserialize;

#[derive(Deserialize)]
pub struct Config {
    pub address: String,
}
