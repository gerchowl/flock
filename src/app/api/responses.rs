use crate::api::schema::{ErrorBody, ErrorResponse, ResponseResult, SuccessResponse};

pub(super) fn encode_success(id: String, result: ResponseResult) -> String {
    serde_json::to_string(&SuccessResponse { id, result }).unwrap()
}

pub(super) fn encode_error(id: String, code: &str, message: impl Into<String>) -> String {
    encode_error_body(
        id,
        ErrorBody {
            code: code.into(),
            message: message.into(),
        },
    )
}

/// An error carrying structured, caller-actionable detail (#329).
///
/// Built directly rather than through [`ErrorBody`]: that type is constructed
/// as a literal in ~25 places, and widening it to carry an optional payload
/// would touch every one of them for the benefit of a single verb. The wire
/// shape is `{"id":…,"error":{"code":…,"message":…,"data":…}}` either way, so
/// clients cannot tell the difference — and `data` stays absent everywhere it
/// is not deliberately populated.
pub(super) fn encode_error_with_data(
    id: String,
    code: &str,
    message: impl Into<String>,
    data: serde_json::Value,
) -> String {
    serde_json::json!({
        "id": id,
        "error": {
            "code": code,
            "message": message.into(),
            "data": data,
        }
    })
    .to_string()
}

pub(super) fn encode_error_body(id: String, error: ErrorBody) -> String {
    serde_json::to_string(&ErrorResponse { id, error }).unwrap()
}
