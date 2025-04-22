use std::path::Path;

use arrow::record_batch::RecordBatch;
use arrow_ipc::writer::FileWriter;
use arrow_schema::Schema;
use std::fs::File;
use std::io::BufWriter;
use tokio::fs::OpenOptions;
use tokio::sync::broadcast;
use tokio::sync::mpsc::Receiver;

use kafkrs_models::message::{arrow_schema, messages_to_recordbatch, Message};

pub struct Writer<'a> {
    file_path: String,
    read_channel: Receiver<Message>,
    shutdown_channel: &'a mut broadcast::Receiver<bool>,
    arrow_writer: FileWriter<BufWriter<File>>,
    buffer: Vec<Message>,
}

async fn file(file_path: &String) -> File {
    let path = Path::new(file_path);
    if !path.exists() {
        panic!("File at {:?} does not exist", file_path)
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .unwrap();
    file.into_std().await
}

impl<'a> Writer<'a> {
    pub async fn new(
        file_path: String,
        read_channel: Receiver<Message>,
        shutdown_channel: &'a mut broadcast::Receiver<bool>,
    ) -> Writer<'a> {
        let file: File = file(&file_path).await;
        let schema: Schema = arrow_schema();
        let arrow_writer: FileWriter<BufWriter<File>> =
            match FileWriter::try_new_buffered(file, &schema) {
                Ok(writer) => writer,
                Err(e) => panic!("Problem opening file: {e:?}"),
            };
        let buffer: Vec<Message> = Vec::new();

        Writer {
            file_path,
            read_channel,
            shutdown_channel,
            arrow_writer,
            buffer,
        }
    }

    pub async fn process(&mut self) {
        loop {
            tokio::select! {
                Some(message) = self.read_channel.recv() => self.process_arrow_message(message).await,
                _ = self.shutdown_channel.recv() => {
                    println!("Shutting down Writer");
                    let _ = self.arrow_writer.flush();
                    let _ = self.arrow_writer.finish();
                    println!("Flushed");
                    break
                }
            }
        }
    }

    async fn process_arrow_message(&mut self, message: Message) {
        self.buffer.push(message);
        if self.buffer.len() == 100 {
            self.process_arrow_messages();
            self.buffer.truncate(0);
        }
    }
    fn process_arrow_messages(&mut self) {
        let batch: RecordBatch = messages_to_recordbatch(&self.buffer);
        self.arrow_writer.write(&batch).unwrap()
    }
}
