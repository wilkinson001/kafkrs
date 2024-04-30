use clap::Parser;
use tokio::net::TcpListener;
use tokio::sync::mpsc::{channel, Receiver, Sender};

use kafkrs_models::message::Message;

use crate::listener::process;
use crate::writer::writer_from_channel;

mod config;
mod listener;
mod writer;

#[derive(Parser)]
struct Cli {
    config_path: Option<String>,
}

#[tokio::main]
async fn main() {
    let args = Cli::parse();
    let config_path: String = args
        .config_path
        .unwrap_or_else(|| String::from("./config.toml"));
    let config = config::load_config(config_path);
    let sock_addr: String = construct_socket_address(config.address, config.port);
    let listener = TcpListener::bind(&sock_addr).await.unwrap();
    println!("Started TCPListener at: {:?}", &sock_addr);
    let (tx, rx): (Sender<Message<String>>, Receiver<Message<String>>) = channel(20);
    tokio::spawn(writer_from_channel(rx, config.logfile));
    loop {
        let (socket, _) = listener.accept().await.unwrap();
        let thread_tx = tx.clone();
        tokio::spawn(async move {
            process(socket, thread_tx).await;
        });
    }
}

fn construct_socket_address(address: String, port: u16) -> String {
    let mut result = address.clone();
    result.push_str(&*":");
    result + &port.to_string()
}
