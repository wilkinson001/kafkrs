use std::path::Path;
use kafkrs_models::config::Config;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::fs;
use tokio::net::TcpListener;

use clap::Parser;


#[derive(Parser)]
struct Cli {
    config_path: Option<String>,
}


#[tokio::main]
async fn main() {
    let args = Cli::parse();
    let config_path: String = if Some(&args.config_path) {
        args.config_path
    } else {
        String::from("./.config.toml")
    };
    let config = load_config(config_path).await;
}


async fn load_config(file_path: String) -> Config {
    let contents: String = fs::read_to_string(Path::new(&file_path)).await?;
    let str_contents: &str = &*contents;
    let config: Config = toml::from_str(str_contents).unwrap();
    return config
}