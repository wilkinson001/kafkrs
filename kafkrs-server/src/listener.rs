use bincode::config::{self, Configuration, Fixint, LittleEndian, NoLimit};
use bincode::serde::decode_from_slice;
use std::collections::HashMap;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

use tokio::sync::mpsc::{channel, Receiver, Sender};

use kafkrs_models::message::Message;

use crate::NewWriterTuple;

pub struct Listener {
    socket: TcpStream,
    write_channel: Sender<Message>,
    new_writer_channel: tokio::sync::broadcast::Receiver<NewWriterTuple>,
    topic_map: HashMap<String, Sender<Message>>,
}

impl Listener {
    pub fn new(
        write_channel: Sender<Message>,
        socket: TcpStream,
        new_writer_channel: tokio::sync::broadcast::Receiver<NewWriterTuple>,
    ) -> Listener {
        //TODO: Add passing in of new_topic_tx channels
        let topic_map = HashMap::new();
        Listener {
            socket,
            write_channel,
            new_writer_channel,
            topic_map,
        }
    }

    async fn handle_topic(&mut self, topic: String) -> Sender<Message> {
        let owned_topic: String = topic.to_owned();
        match self.topic_map.get(&owned_topic) {
            Some(res) => res.to_owned(),
            None => {
                let (tx, _rx): (Sender<Message>, Receiver<Message>) = channel(20);
                let cloned_tx = tx.clone();
                self.topic_map.insert(owned_topic.clone(), cloned_tx);
                tx
            }
        }
    }

    async fn listen_to_sock(
        &mut self,
        buffer: &mut Vec<u8>,
        bin_conf: Configuration<LittleEndian, Fixint, NoLimit>,
    ) {
        //TODO: Update to select tx channel based on topic
        let permit = self.write_channel.reserve().await.unwrap();
        let _ = self.socket.read_to_end(buffer).await;
        let message: Message = decode_from_slice(buffer, bin_conf).unwrap().0;
        println!(
            "{:?} - Received Message with key: {:?}",
            message.timestamp, message.key
        );
        permit.send(message);
    }

    pub async fn process(&mut self, cancellation_token: CancellationToken) {
        let mut buffer: Vec<u8> = Vec::new();
        let bin_conf: Configuration<LittleEndian, Fixint, NoLimit> = config::legacy();
        loop {
            {
                tokio::select! {
                    () = self.listen_to_sock(&mut buffer, bin_conf) => {
                        println!("Socket closed, exiting Listener")
                    },
                    () = cancellation_token.cancelled() => {
                        println!("Shutting down Listener");
                        break
                    }
                }
            }
        }
    }
}
