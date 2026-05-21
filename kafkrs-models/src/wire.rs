//! Generated wire-protocol types. The schema lives in
//! `kafkrs-models/proto/wire/v1.proto` and is compiled at build time by
//! `build.rs`.

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/kafkrs.wire.v1.rs"));
}
