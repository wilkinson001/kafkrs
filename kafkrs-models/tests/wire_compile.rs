//! Smoke test that the generated wire types are accessible.

#[test]
fn command_type_exists() {
    let cmd = kafkrs_models::wire::v1::Command {
        correlation_id: 42,
        body: None,
    };
    assert_eq!(cmd.correlation_id, 42);
}

#[test]
fn error_code_enum_values_match_spec() {
    use kafkrs_models::wire::v1::ErrorCode;
    assert_eq!(ErrorCode::ErrUnsupportedProtocolVersion as i32, 100);
    assert_eq!(ErrorCode::ErrUnknownTopic as i32, 200);
    assert_eq!(ErrorCode::ErrTopicAlreadyExists as i32, 300);
    assert_eq!(ErrorCode::ErrInternal as i32, 900);
}
