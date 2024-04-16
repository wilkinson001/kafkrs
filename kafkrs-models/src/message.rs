use chrono::{DateTime, Utc};
use schema::Schema;
use crate::schema;

struct RawMessage<T> {
    key: Vec<u8>,
    value: Vec<u8>,
    timestamp: i64,
    schema: Option<Schema<T>>
}

struct Message<T> {
    key: String,
    value: T,
    timestamp: DateTime<Utc>
}


pub trait Decode<T> {
    fn decode(self) -> Message<T>;
}


pub trait Deserialse<T> {
    fn deserialise(self, value: Vec<u8>) -> T;
}

impl<T> Decode<T> for RawMessage<T> where T: Deserialse<T>
{
    fn decode(mut self) -> Message<T> {
        let key = String::from_utf8(self.key).unwrap();
        let timestamp = DateTime::from_timestamp_nanos(self.timestamp);
        let schema = self.schema.unwrap();
        let value = schema.def.deserialise(self.value);
        return Message{key, value, timestamp};

    }
}

impl Deserialse<String> for String {
    fn deserialise(self, value: Vec<u8>) -> String {
        String::from_utf8(value).unwrap()
    }
}