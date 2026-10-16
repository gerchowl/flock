use crate::api::schema::{ErrorBody, ErrorResponse, ResponseResult, SuccessResponse};

pub(crate) fn encode_success(id: String, result: ResponseResult) -> String {
    serde_json::to_string(&SuccessResponse { id, result }).unwrap()
}

pub(crate) fn encode_error(id: String, code: &str, message: impl Into<String>) -> String {
    encode_error_body(
        id,
        ErrorBody {
            data: None,
            code: code.into(),
            message: message.into(),
        },
    )
}

/// An error carrying structured, caller-actionable detail (#329).
///
/// Encodes arbitrary method-specific data without requiring a typed producer.
/// `ErrorBody` also decodes this additive data, including input-write evidence.
pub(crate) fn encode_error_with_data(
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

pub(crate) fn encode_allocation_plan(
    id: String,
    operation: &str,
    plan: serde_json::Value,
) -> String {
    encode_success(
        id,
        ResponseResult::AllocationPlan {
            operation: operation.into(),
            plan,
        },
    )
}
