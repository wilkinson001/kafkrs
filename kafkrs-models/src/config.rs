use serde::Deserialize;

#[derive(Deserialize)]
pub struct Config {
    address: String
}