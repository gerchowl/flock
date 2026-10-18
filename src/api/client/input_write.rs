//! ADR-0028 §5a: capability-bound writes, one caller budget and no replay.
use super::*;
use crate::api::{error_code::ErrorCode, schema::PaneSendTextParams};
use serde_json::{json, Value};

/// Default bounded client wait. Raw writes reserve a reply margin within it.
pub(crate) const CAP: Duration = Duration::from_secs(10);
const REPLY_MARGIN: Duration = Duration::from_secs(1);
/// Guarded submit reserves the server's complete wait bound, one second for
/// capability admission and one for the final reply: 11470 ms in this build.
pub(crate) const SUBMIT_CAP: Duration =
    crate::app::guarded_submit::SUBMIT_WAIT_BOUND.saturating_add(REPLY_MARGIN.saturating_mul(2));

fn guarded(method: &Method) -> bool {
    matches!(method, Method::PaneSubmit(_)) || matches!(method, Method::AgentSend(p) if p.submit)
}

pub(super) fn cap(method: &Method) -> Duration {
    if guarded(method) {
        SUBMIT_CAP
    } else {
        CAP
    }
}

pub(super) fn requires(method: &Method) -> bool {
    matches!(
        method,
        Method::PaneSendText(_)
            | Method::PaneSendKeys(_)
            | Method::PaneSendInput(_)
            | Method::PaneWriteText(_)
            | Method::PaneWriteKeys(_)
            | Method::PaneWriteInput(_)
            | Method::PaneSubmit(_)
    ) || matches!(method, Method::AgentSend(_))
}

fn refusal(request: &Request, code: ErrorCode, message: &str) -> Value {
    json!({"id":request.id,"error":{"code":code.as_str(),"message":message}})
}

/// Budget selection is pure so exact boundaries do not depend on a wall clock.
fn bind(request: &Request, remaining: Duration) -> Result<Request, Value> {
    let mut bound = request.clone();
    bound.method = match bound.method {
        Method::PaneSendText(p) => Method::PaneWriteText(p),
        Method::PaneSendKeys(p) => Method::PaneWriteKeys(p),
        Method::PaneSendInput(p) => Method::PaneWriteInput(p),
        method => method,
    };
    let invalid = || {
        refusal(
            request,
            ErrorCode::InvalidParams,
            "caller budget cannot accommodate the write deadline and reply margin",
        )
    };
    if guarded(&bound.method) {
        if remaining <= crate::app::guarded_submit::SUBMIT_WAIT_BOUND + REPLY_MARGIN {
            return Err(invalid());
        }
        return Ok(bound);
    }
    // Paste observation follows a completed text write and needs its own room.
    let observation = if matches!(bound.method, Method::PaneWriteText(_)) {
        crate::app::guarded_submit::CONFIRM_WINDOW.as_millis()
    } else {
        0
    };
    let timeout = match &mut bound.method {
        Method::PaneWriteText(p) => &mut p.write_timeout_ms,
        Method::PaneWriteKeys(p) => &mut p.write_timeout_ms,
        Method::PaneWriteInput(p) => &mut p.write_timeout_ms,
        _ => unreachable!("raw agent targets are resolved before binding"),
    };
    let requested = match timeout {
        None => 8000,
        Some(value) => value
            .as_u64()
            .filter(|ms| (1..=30000).contains(ms))
            .ok_or_else(|| {
                refusal(
                    request,
                    ErrorCode::InvalidParams,
                    "write_timeout_ms must be an integer in 1..=30000",
                )
            })?,
    };
    let available = remaining
        .as_millis()
        .saturating_sub(REPLY_MARGIN.as_millis() + observation + 1);
    let selected = u128::from(requested).min(available);
    if selected == 0 {
        return Err(invalid());
    }
    *timeout = Some(json!(selected as u64));
    Ok(bound)
}

/// One fresh protocol/capability observation within the original caller budget.
/// A socket pathname cache cannot bind a write across a server replacement.
pub(super) fn call(
    client: &ApiClient,
    request: &Request,
    cap: Duration,
    deadline: Option<Instant>,
) -> Result<Value, ApiClientError> {
    let end = deadline.map_or(Instant::now() + cap, |end| end.min(Instant::now() + cap));
    let left = || end.saturating_duration_since(Instant::now());
    let mut request = request.clone();
    // Refuse exhausted caller budgets before even the capability query.
    if left() <= Duration::from_millis(1001) {
        return Ok(refusal(
            &request,
            ErrorCode::InvalidParams,
            "caller budget exhausted before input dispatch",
        ));
    }
    let ping = client
        .exchange_until(
            &Request {
                id: "api:input-write-capability".into(),
                method: Method::Ping(PingParams::default()),
            },
            end,
        )
        .map_err(Attempt::into_error)?;
    // The fresh ping binds both protocol and capability to this server. Interpret
    // skew first, preserving exit 78 before considering capability refusal.
    if let Some(refusal) = crate::api::compatibility::refuse_skew(client, &request) {
        return Ok(refusal);
    }
    if ping
        .pointer("/result/capabilities/pane_input_write_ack")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Ok(refusal(
            &request,
            ErrorCode::ServerVersionGap,
            "completed input writes require pane_input_write_ack; hand off or upgrade the server",
        ));
    }
    // Both verbs use the server's terminal-target resolver, including shells.
    // agent.get is narrower: it would incorrectly reject a shell placement.
    // Guarded agent.send keeps its verb so the server restores the operator draft.
    let agent_target = match &request.method {
        Method::AgentSend(params) if !params.submit => {
            let target = params.target.clone();
            request.method = Method::PaneWriteText(PaneSendTextParams {
                pane_id: target.clone(),
                text: params.text.clone(),
                raw: params.raw,
                write_timeout_ms: params.write_timeout_ms.clone(),
            });
            Some(target)
        }
        _ => None,
    };
    let bound = match bind(&request, left()) {
        Ok(bound) => bound,
        Err(error) => return Ok(error),
    };
    // No retries, including connect failures. After dispatch, only a receipt
    // can establish counts or prove cancellation; EOF and malformed replies cannot.
    match client.exchange_until(&bound, end) {
        Ok(mut response) => {
            if let Some(target) =
                agent_target.filter(|_| response["error"]["code"] == "pane_not_found")
            {
                if let Some(error) = response.get_mut("error").and_then(Value::as_object_mut) {
                    if let Some(data) = error.get_mut("data").and_then(Value::as_object_mut) {
                        data.remove("expected");
                        if data.is_empty() {
                            error.remove("data");
                        }
                    }
                }
                response["error"]["code"] = json!(ErrorCode::AgentNotFound.as_str());
                response["error"]["message"] = json!(format!("agent target {target} not found"));
            }
            match parse_response_value(response.clone()) {
                Ok(reply)
                    if reply.id == request.id && final_result(&bound.method, &reply.result) =>
                {
                    Ok(response)
                }
                Err(ApiClientError::ErrorResponse(reply)) if reply.id == request.id => Ok(response),
                _ => Ok(unknown(&request, "invalid_reply")),
            }
        }
        Err(Attempt::BeforeSend(error)) => Err(error),
        Err(Attempt::AfterSend(
            ApiClientError::Json(_) | ApiClientError::ReplyLineTooLarge { .. },
        )) => Ok(unknown(&request, "invalid_reply")),
        Err(Attempt::AfterSend(_)) => Ok(unknown(&request, "transport_lost")),
    }
}

fn final_result(method: &Method, result: &ResponseResult) -> bool {
    if guarded(method) {
        return matches!(result, ResponseResult::GuardedSubmit { .. });
    }
    matches!(
        (method, result),
        (
            Method::PaneWriteText(_),
            ResponseResult::Paste { .. } | ResponseResult::Ok {}
        ) | (
            Method::PaneWriteInput(_) | Method::PaneWriteKeys(_),
            ResponseResult::Ok {}
        )
    )
}

fn unknown(request: &Request, reason: &str) -> Value {
    let mut error = refusal(
        request,
        ErrorCode::InputWriteUnknown,
        "input effect unknown after transport loss; inspect the pane before another operation",
    );
    error["error"]["data"] = json!({"input_write":{
        "bytes_written":null,"bytes_total":null,"reason":reason,"remainder_discarded":false}});
    error
}

/// Normalize only an incapable method tag, including stale capability observations.
pub(super) fn normalize(method: &Method, response: &mut Value) -> bool {
    let tag = match method {
        Method::PaneWriteText(_) => "pane.write_text",
        Method::PaneWriteKeys(_) => "pane.write_keys",
        Method::PaneWriteInput(_) => "pane.write_input",
        Method::PaneSubmit(_) => "pane.submit",
        Method::AgentSend(p) if p.submit => "agent.send",
        _ => return false,
    };
    let error = &response["error"];
    let message = error["message"].as_str().unwrap_or_default();
    if matches!(
        error["code"].as_str(),
        Some("unknown_method" | "method_not_found")
    ) || error["code"].as_i64() == Some(-32601)
        || (error["code"] == "invalid_request"
            && message.contains(&format!("unknown variant `{tag}`")))
    {
        response["error"] = json!({"code":ErrorCode::ServerVersionGap.as_str(),
            "message":"server does not implement the completed input write method; upgrade before retrying"});
        return true;
    }
    false
}

#[cfg(test)]
mod tests;
