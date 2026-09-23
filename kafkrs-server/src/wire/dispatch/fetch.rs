//! `Fetch` handler. Reads through the fetcher's three-tier resolution
//! (active batch → in-flight upload queue → object store) with a long-poll
//! bounded by the topic's `max_fetch_wait_ms`.

use super::SharedState;
use crate::fetcher::{fetch, FetchRequest};
use crate::metrics::{
    partition_label, partition_label_with, FETCH_BYTES, FETCH_ERRORS, FETCH_LATENCY_MS,
    FETCH_RECORDS, FETCH_REQUESTS, LABEL_ERROR_CODE,
};
use crate::wire::errors::{fetch_error_code, make_error};
use crate::wire::frame::Frame;
use bytes::Bytes;
use kafkrs_models::wire::v1::{command::Body, Command, ErrorCode, FetchResponse, OutRecordMeta};

pub async fn handle_fetch(
    correlation_id: u64,
    state: &SharedState,
    req: kafkrs_models::wire::v1::FetchRequest,
) -> Frame {
    let __start = std::time::Instant::now();
    let __topic = req.topic.clone();
    let __partition = req.partition;
    metrics::counter!(FETCH_REQUESTS, &partition_label(&__topic, __partition)).increment(1);

    let handle = {
        let guard = state.partitions.read().await;
        guard.get(&(req.topic.clone(), req.partition)).cloned()
    };
    let Some(handle) = handle else {
        metrics::counter!(
            FETCH_ERRORS,
            &partition_label_with(
                &__topic,
                __partition,
                &[(
                    LABEL_ERROR_CODE,
                    format!("{}", ErrorCode::ErrUnknownTopic as i32)
                )],
            )
        )
        .increment(1);
        return Frame {
            command: make_error(correlation_id, ErrorCode::ErrUnknownTopic, ""),
            payload: Bytes::new(),
        };
    };
    let effective_wait = (req.max_wait_ms as u64).min(handle.cfg.max_fetch_wait_ms);
    let result = fetch(
        FetchRequest {
            topic: req.topic,
            topic_uuid: handle.uuid.clone(),
            partition: req.partition,
            from_offset: req.from_offset,
            max_records: req.max_records as usize,
            max_wait_ms: effective_wait,
        },
        &handle.pw_tx,
        &handle.tail,
        &state.store,
        &state.prefix,
    )
    .await;
    let resp = match result {
        Ok(r) => r,
        Err(e) => {
            let err_code = fetch_error_code(&e);
            metrics::counter!(
                FETCH_ERRORS,
                &partition_label_with(
                    &__topic,
                    __partition,
                    &[(LABEL_ERROR_CODE, format!("{}", err_code as i32))],
                )
            )
            .increment(1);
            return Frame {
                command: make_error(correlation_id, err_code, ""),
                payload: Bytes::new(),
            };
        }
    };
    // Build payload + metas.
    let mut payload = bytes::BytesMut::new();
    let mut metas = Vec::with_capacity(resp.records.len());
    for r in &resp.records {
        metas.push(OutRecordMeta {
            offset: r.offset,
            timestamp_ns: r.timestamp_ns,
            schema_id: r.schema_id,
            key_len: r.key.len() as u32,
            value_len: r.value.len() as u32,
        });
        payload.extend_from_slice(&r.key);
        payload.extend_from_slice(&r.value);
    }
    let returned_records_count = resp.records.len() as u64;
    let returned_bytes: u64 = resp
        .records
        .iter()
        .map(|r| (r.key.len() + r.value.len()) as u64)
        .sum();
    let __labels = partition_label(&__topic, __partition);
    metrics::counter!(FETCH_RECORDS, &__labels).increment(returned_records_count);
    metrics::counter!(FETCH_BYTES, &__labels).increment(returned_bytes);
    metrics::histogram!(FETCH_LATENCY_MS, &__labels)
        .record(__start.elapsed().as_secs_f64() * 1000.0);
    Frame {
        command: Command {
            correlation_id,
            body: Some(Body::FetchResp(FetchResponse {
                records: metas,
                hwm: resp.hwm,
            })),
        },
        payload: payload.freeze(),
    }
}
