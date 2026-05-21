//! Maps internal broker errors into the wire's structured ErrorCode taxonomy.

use crate::fetcher::FetchError;
use crate::topic_registry::RegistryError;
use kafkrs_models::wire::v1::{command::Body, Command, ErrorCode, ErrorResponse};

/// Build an Error Command with the given code, message, and echo correlation_id.
pub fn make_error(correlation_id: u64, code: ErrorCode, message: impl Into<String>) -> Command {
    Command {
        correlation_id,
        body: Some(Body::Error(ErrorResponse {
            code: code as i32,
            message: message.into(),
        })),
    }
}

pub fn fetch_error_code(e: &FetchError) -> ErrorCode {
    match e {
        FetchError::UnknownTopic => ErrorCode::ErrUnknownTopic,
        FetchError::UnknownPartition => ErrorCode::ErrUnknownPartition,
        FetchError::OffsetOutOfRange => ErrorCode::ErrOffsetOutOfRange,
        FetchError::BrokerNotReady => ErrorCode::ErrBrokerNotReady,
    }
}

pub fn registry_error_code(e: &RegistryError) -> ErrorCode {
    match e {
        RegistryError::AlreadyExists => ErrorCode::ErrTopicAlreadyExists,
        RegistryError::Io(_) => ErrorCode::ErrInternal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_error_mapping_is_total_and_correct() {
        assert_eq!(
            fetch_error_code(&FetchError::UnknownTopic),
            ErrorCode::ErrUnknownTopic
        );
        assert_eq!(
            fetch_error_code(&FetchError::UnknownPartition),
            ErrorCode::ErrUnknownPartition
        );
        assert_eq!(
            fetch_error_code(&FetchError::OffsetOutOfRange),
            ErrorCode::ErrOffsetOutOfRange
        );
        assert_eq!(
            fetch_error_code(&FetchError::BrokerNotReady),
            ErrorCode::ErrBrokerNotReady
        );
    }

    #[test]
    fn registry_already_exists_maps_to_topic_already_exists() {
        assert_eq!(
            registry_error_code(&RegistryError::AlreadyExists),
            ErrorCode::ErrTopicAlreadyExists,
        );
    }

    #[test]
    fn registry_io_maps_to_internal() {
        assert_eq!(
            registry_error_code(&RegistryError::Io("disk full".into())),
            ErrorCode::ErrInternal,
        );
    }

    #[test]
    fn make_error_sets_correlation_id_and_body() {
        let cmd = make_error(123, ErrorCode::ErrUnknownTopic, "no such topic");
        assert_eq!(cmd.correlation_id, 123);
        match cmd.body {
            Some(Body::Error(er)) => {
                assert_eq!(er.code, ErrorCode::ErrUnknownTopic as i32);
                assert_eq!(er.message, "no such topic");
            }
            _ => panic!("expected Error body"),
        }
    }
}
