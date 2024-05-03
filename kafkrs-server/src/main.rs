use clap::Parser;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::broadcast;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::task::JoinSet;

use kafkrs_models::message::Message;

use crate::listener::Listener;
use crate::writer::Writer;

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

    let (shutdown_tx, mut shutdown_rx): (broadcast::Sender<bool>, broadcast::Receiver<bool>) =
        broadcast::channel(1);

    let (tx, rx): (Sender<Message<String>>, Receiver<Message<String>>) = channel(20);
    let mut set = JoinSet::new();
    set.spawn(async move {
        let mut writer = Writer::new(config.logfile, rx, &mut shutdown_rx);
        writer.process().await
    });
    set.spawn(async move {
        let sock_addr: String = construct_socket_address(config.address, config.port);
        let tcp_listener = TcpListener::bind(&sock_addr).await.unwrap();
        println!("Started TCPListener at: {:?}", &sock_addr);
        loop {
            let (socket, _) = tcp_listener.accept().await.unwrap();
            let thread_tx = tx.clone();
            tokio::spawn(async move {
                let mut listener = Listener::new(thread_tx, socket);
                listener.process().await
            });
        }
    });
    match signal::ctrl_c().await {
        Ok(()) => {
            println!("Shutdown signal received");
            _ = shutdown_tx.send(true);
            if let Some(_) = set.join_next().await {
                println!("Shutdown complete. Goodbye")
            }
        }
        Err(err) => eprintln!("Unable to listen for shutdown signal: {}", err),
    }
}

fn construct_socket_address(address: String, port: u16) -> String {
    let mut result = address.clone();
    result.push_str(&*":");
    result + &port.to_string()
}
