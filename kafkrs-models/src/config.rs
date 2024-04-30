use serde::Deserialize;

#[derive(Deserialize)]
pub struct Config {
    pub address: String,
    pub port: u16,
    pub logfile: String,
}
