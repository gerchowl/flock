use super::ErrorCode;

#[test]
fn compatibility_note_belongs_to_the_unsupported_permission_decision() {
    let documented: Vec<_> = ErrorCode::DOCUMENTATION
        .iter()
        .filter(|(_, docs)| {
            docs.iter()
                .any(|doc| doc.contains("predating session grants"))
        })
        .map(|(code, _)| *code)
        .collect();
    assert_eq!(documented, [ErrorCode::PermissionDecisionUnsupported]);
}
