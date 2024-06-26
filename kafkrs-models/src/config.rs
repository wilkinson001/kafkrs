use serde::Deserialize;

#[derive(Deserialize)]
pub struct Config {
    pub address: String,
    pub ports: Vec<u16>,
    pub logfile: String,
}
