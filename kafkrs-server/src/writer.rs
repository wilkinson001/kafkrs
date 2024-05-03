use std::path::Path;

use bincode::config;
use bincode::serde::encode_to_vec;
use serde::Serialize;
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::broadcast;
use tokio::sync::mpsc::Receiver;

use kafkrs_models::message::Message;

pub struct Writer<'a, T> {
    file_path: String,
    read_channel: Receiver<Message<T>>,
    shutdown_channel: &'a mut broadcast::Receiver<bool>,
}

impl<'a, T: Serialize> Writer<'a, T> {
    pub fn new(
        file_path: String,
        read_channel: Receiver<Message<T>>,
        shutdown_channel: &mut broadcast::Receiver<bool>,
    ) -> Writer<T> {
        Writer {
            file_path,
            read_channel,
            shutdown_channel,
        }
    }
    async fn file(&mut self) -> File {
        let path = Path::new(&self.file_path);
        if !path.exists() {
            panic!("File at {:?} does not exist", self.file_path)
        }
        File::open(path).await.unwrap()
    }
    async fn process_message(&mut self, buf_writer: &mut BufWriter<File>, message: Message<T>) {
        let bin_conf = config::legacy();
        println!("Writing Message: {:?}", message.key);
        let de_mes = encode_to_vec(&message, bin_conf).unwrap();
        let _ = buf_writer.write(&de_mes).await;
        println!("Message {:?} written successfully!", message.key)
    }

    pub async fn process(&mut self) {
        let mut buf_writer = BufWriter::new(self.file().await);
        loop {
            tokio::select! {
                Some(message) = self.read_channel.recv() => self.process_message(&mut buf_writer, message).await,
                _ = self.shutdown_channel.recv() => {
                    println!("Shutting down Writer");
                    _ = buf_writer.shutdown().await;
                    println!("Flushed");
                    break
                }
            }
        }
    }
}
