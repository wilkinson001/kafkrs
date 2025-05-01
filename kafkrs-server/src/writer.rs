use std::path::Path;

use arrow::record_batch::RecordBatch;
use arrow_ipc::writer::FileWriter;
use arrow_schema::Schema;
use std::fs::File;
use std::io::BufWriter;
use tokio::fs::OpenOptions;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;

use kafkrs_models::message::{arrow_schema, messages_to_recordbatch, Message};

pub struct Writer {
    file_path: String,
    read_channel: Receiver<Message>,
    cancellation_token: CancellationToken,
    topic: String,
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

impl Writer {
    pub async fn new(
        file_path: String,
        read_channel: Receiver<Message>,
        cancellation_token: CancellationToken,
        topic: String,
    ) -> Writer {
        let mut file_name: String = file_path.to_owned();
        let owned_topic: String = topic.to_owned();
        file_name.push_str(&owned_topic);
        let file: File = file(&file_name).await;
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
            cancellation_token,
            topic,
            arrow_writer,
            buffer,
        }
    }

    pub async fn process(&mut self) {
        loop {
            tokio::select! {
                Some(message) = self.read_channel.recv() => self.process_arrow_message(message).await,
                () = self.cancellation_token.cancelled() => {
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
