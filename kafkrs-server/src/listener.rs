use bincode::config;
use bincode::serde::decode_from_slice;
use serde::de::DeserializeOwned;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc::Sender;

use kafkrs_models::message::Message;

pub async fn process<T: DeserializeOwned>(mut socket: TcpStream, tx: Sender<Message<T>>) {
    let permit = tx.reserve().await.unwrap();
    let mut buffer: Vec<u8> = Vec::new();
    let _ = socket.read_to_end(&mut buffer).await;
    let bin_conf = config::legacy();
    let message: Message<T> = decode_from_slice(&buffer, bin_conf).unwrap().0;
    println!(
        "{:?} - Received Message with key: {:?}",
        message.timestamp, message.key
    );
    permit.send(message);
}
