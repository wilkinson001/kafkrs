mod config;
mod server;

use clap::Parser;

#[derive(Parser)]
struct Cli {
    config_path: Option<String>,
}
#[tokio::main]
async fn main() {
    let args = Cli::parse();
    let config_path: String = match args.config_path {
        Some(x) => x,
        None => String::from("./config.toml"),
    };
    let config = config::load_config(config_path);
}
