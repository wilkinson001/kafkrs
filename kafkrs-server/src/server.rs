use std::io::Result;
use std::path::Path;

use bincode::config;
use bincode::serde::encode_to_vec;
use serde::ser::Serialize;
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::mpsc::Receiver;

use kafkrs_models::message::Message;

async fn writer_from_channel<T: Serialize>(mut rx: Receiver<Message<T>>, file: Box<Path>) {
    let file = get_or_create_file(file).await.unwrap();
    let mut buf_writer = BufWriter::new(file);
    let bin_conf = config::legacy();
    while let Some(message) = rx.recv().await {
        let de_mes = encode_to_vec(&message, bin_conf).unwrap();
        let _ = buf_writer.write(&de_mes);
    }
    let _ = buf_writer.flush();
}

async fn get_or_create_file(file_path: Box<Path>) -> Result<File> {
    if !file_path.exists() {
        panic!("File at {:?} does not exist", file_path)
    }
    return File::open(file_path).await;
}
