use clap::Parser;
use kafkrs_models::config::Config;
use kafkrs_models::message::Message;
use log::{error, info};
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio_util::sync::CancellationToken;
use tokio_util::task::task_tracker::TaskTracker;

use crate::listener::Listener;
use crate::writer::Writer;

mod config;
mod listener;
mod writer;

#[derive(Parser)]
struct Cli {
    config_path: Option<String>,
}

type NewWriterTuple = (String, Sender<Message>);

#[tokio::main]
async fn main() {
    let args = Cli::parse();
    let config_path: String = args
        .config_path
        .unwrap_or_else(|| String::from("./config.toml"));
    let config: Config = config::load_config(config_path);

    let (tx, rx): (Sender<Message>, Receiver<Message>) = channel(20);
    let (new_topic_tx, mut new_topic_rx): (Sender<String>, Receiver<String>) = channel(20);
    let (new_writer_tx, new_writer_rx): (
        tokio::sync::broadcast::Sender<NewWriterTuple>,
        tokio::sync::broadcast::Receiver<NewWriterTuple>,
    ) = tokio::sync::broadcast::channel(20);

    let tracker = TaskTracker::new();
    let token = CancellationToken::new();
    generate_new_writer(
        String::from("default"),
        tracker.clone(),
        token.clone(),
        &config,
        new_writer_tx.clone(),
    )
    .await;

    for port in config.ports.clone() {
        let mov_address = config.address.clone();
        let thread_tx = tx.clone();
        let cloned_writer_rx = new_writer_tx.subscribe();
        let cloned_token = token.clone();
        tracker.spawn(async move {
            let sock_addr: String = construct_socket_address(mov_address, port);
            let listener = TcpListener::bind(&sock_addr).await.unwrap();
            info!("Started TCPListener at: {:?}", &sock_addr);
            let (socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut listener = Listener::new(thread_tx, socket, cloned_writer_rx);
                listener.process(cloned_token).await
            });
        });
    }
    {
        let tracker = tracker.clone();
        let token = token.clone();
        tokio::spawn(async move {
            match signal::ctrl_c().await {
                Ok(()) => {
                    println!("Shutdown signal received");
                    tracker.close();
                    token.cancel();
                    tracker.wait().await;
                }
                Err(err) => error!("Unable to listen for shutdown signal: {}", err),
            }
        });
    }

    loop {
        tokio::select! {
            Some(topic) = new_topic_rx.recv() => generate_new_writer(topic, tracker.clone(), token.clone(), &config, new_writer_tx.clone()).await,
            () = token.cancelled() => {
                println!("Shutting down Main");
                break
            }
        }
    }
}

fn construct_socket_address(address: String, port: u16) -> String {
    let mut result = address.clone();
    result.push(':');
    result + &port.to_string()
}

async fn generate_new_writer(
    topic: String,
    tracker: TaskTracker,
    token: CancellationToken,
    config: &Config,
    new_writer_tx: tokio::sync::broadcast::Sender<(String, Sender<Message>)>,
) {
    let (tx, rx): (Sender<Message>, Receiver<Message>) = channel(20);
    let logfile = config.logfile.clone();
    let cloned_topic = topic.clone();
    tracker.spawn(async move {
        let mut writer = Writer::new(logfile, rx, token, cloned_topic).await;
        writer.process().await
    });
    new_writer_tx.send((topic.clone(), tx));
}
