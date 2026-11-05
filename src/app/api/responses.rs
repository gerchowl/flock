use crate::api::schema::{ErrorBody, ErrorResponse, ResponseResult, SuccessResponse};

pub(crate) fn encode_success(id: String, result: ResponseResult) -> String {
    serde_json::to_string(&SuccessResponse { id, result }).unwrap()
}

pub(crate) fn encode_error(
    id: String,
    code: &str,
    message: impl Into<crate::process::CommandFailure>,
) -> String {
    let message = message.into();
    encode_error_body(
        id,
        ErrorBody {
            data: message.data.map(|extra| {
                Box::new(crate::api::schema::ErrorData {
                    input_write: None,
                    extra,
                })
            }),
            code: code.into(),
            message: message.message,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{CommandFailure, TracedCommand};
    use crate::worktree::{
        explain_worktree_add_failure, run_command_capture, run_worktree_command, WorktreeCommand,
    };

    fn response(error: impl Into<CommandFailure>) -> ErrorResponse {
        serde_json::from_str(&encode_error(
            "test".into(),
            "worktree_create_failed",
            error,
        ))
        .unwrap()
    }

    #[test]
    fn missing_program_names_dependency_and_preserves_api_data() {
        let dir = crate::test_support::TempDir::socket();
        let program = dir.path().join("missing-git").display().to_string();
        let command = WorktreeCommand {
            program: program.clone(),
            args: vec![],
        };
        let failure = run_worktree_command(&command).unwrap_err();
        let error = response(explain_worktree_add_failure("HEAD", failure)).error;
        assert_eq!(error.code, "worktree_create_failed");
        assert!(error.message.contains(&program), "{}", error.message);
        assert!(error.message.contains("server's PATH"));
        let data = error.data.unwrap().extra;
        assert_eq!(data["reason"], "program_not_found");
        assert_eq!(data["program"], program);
        assert_eq!(
            data["path_length"],
            std::env::var_os("PATH").map_or(0, |path| path.len())
        );
        assert!(!data.contains_key("PATH"));
        assert!(!data.contains_key("path"));
    }

    #[test]
    fn missing_current_dir_is_distinct_from_missing_program() {
        let dir = crate::test_support::TempDir::socket();
        let missing = dir.path().join("missing-checkout");
        let error = response(
            run_command_capture("/bin/sh", &["-c", "exit 0"], Some(&missing)).unwrap_err(),
        )
        .error;
        assert!(error.message.contains("working directory"));
        assert!(!error.message.contains("program_not_found"));
        let data = error.data.unwrap().extra;
        assert_eq!(data["reason"], "current_dir_not_found");
        assert_eq!(data["program"], "/bin/sh");
        assert_eq!(data["current_dir"], missing.to_str().unwrap());
    }

    #[test]
    fn filesystem_and_git_path_errors_keep_their_original_messages() {
        let dir = crate::test_support::TempDir::socket();
        let missing = dir.path().join("missing-checkout");
        // The same ENOENT from a launch has evidence, unlike a filesystem read.
        let launch = TracedCommand::new("/bin/sh", "test")
            .current_dir(&missing)
            .output_traced()
            .unwrap_err();
        assert!(response(CommandFailure::from(launch)).error.data.is_some());
        let path_error = std::fs::read(&missing).unwrap_err();
        let text = path_error.to_string();
        let error = response(CommandFailure::from(path_error)).error;
        assert_eq!(error.message, text);
        assert!(error.data.is_none());
        let expected = TracedCommand::new("git", "test")
            .arg("-C")
            .arg(&missing)
            .arg("status")
            .output_traced()
            .unwrap();
        assert!(!expected.status.success());
        let path = missing.to_str().unwrap();
        let error =
            response(run_command_capture("git", &["-C", path, "status"], None).unwrap_err()).error;
        assert_eq!(
            error.message,
            String::from_utf8_lossy(&expected.stderr).trim()
        );
        assert!(error.data.is_none());
    }
}
