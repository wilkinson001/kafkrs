use bincode::config;
use bincode::serde::decode_from_slice;
use serde::de::DeserializeOwned;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc::Sender;

use kafkrs_models::message::Message;

pub struct Listener<T> {
    socket: TcpStream,
    write_channel: Sender<Message<T>>,
}

impl<T: DeserializeOwned> Listener<T> {
    pub fn new(write_channel: Sender<Message<T>>, socket: TcpStream) -> Listener<T> {
        Listener {
            socket,
            write_channel,
        }
    }

    pub async fn process(&mut self) {
        let mut buffer: Vec<u8> = Vec::new();
        let bin_conf = config::legacy();
        loop {
            let permit = self.write_channel.reserve().await.unwrap();
            let _ = self.socket.read_to_end(&mut buffer).await;
            let message: Message<T> = decode_from_slice(&buffer, bin_conf).unwrap().0;
            println!(
                "{:?} - Received Message with key: {:?}",
                message.timestamp, message.key
            );
            permit.send(message);
        }
    }
}
