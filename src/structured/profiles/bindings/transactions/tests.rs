use super::*;

fn fixture() -> (ProfileAdmission, std::path::PathBuf) {
    let root = crate::test_support::unique_temp_path("binding-operations");
    fs::create_dir_all(&root).unwrap();
    let gate = ProfileAdmission::load(&root.join("profile-bindings.json"));
    (gate, root)
}

fn request(id: &str) -> PlanRequest {
    serde_json::from_value(json!({"action":"bind","binding_id":id,
        "target":{"repo_id":"repo-fixture","node_ids":["node-fixture"],"agent_ids":["agent-fixture"]},
        "account_alias":"fixture-account","credential_ref":"fixture-reference","credential_generation":1,
        "bundle_digests":["fixture-digest"],"grants":[]})).unwrap()
}

fn bind(gate: &mut ProfileAdmission, plan: &Value, principal: u32) -> Result<Value, E> {
    gate.bind(
        principal,
        plan["plan"].as_str().unwrap(),
        plan["expect"].as_str().unwrap(),
    )
}

#[test]
fn expiry_and_tampered_review_refuse_before_persistence() {
    let (mut gate, root) = fixture();
    let plan = gate.plan_bind(7, request("binding-a")).unwrap();
    gate.plans
        .get_mut(plan["plan"].as_str().unwrap())
        .unwrap()
        .deadline = Duration::ZERO;
    assert_eq!(bind(&mut gate, &plan, 7), Err(E::ProfilePlanExpired));
    assert!(!root.join("profile-bindings.json").exists());
    let plan = gate.plan_bind(7, request("binding-a")).unwrap();
    gate.plans
        .get_mut(plan["plan"].as_str().unwrap())
        .unwrap()
        .review["binding"]["credential_ref"] = json!("altered-reference");
    assert_eq!(bind(&mut gate, &plan, 7), Err(E::ProfilePlanInvalid));
    assert!(!root.join("profile-bindings.json").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn principal_is_bound_to_plan_and_durable_receipt() {
    let (mut gate, root) = fixture();
    let plan = gate.plan_bind(7, request("binding-a")).unwrap();
    assert_eq!(bind(&mut gate, &plan, 8), Err(E::ProfilePlanInvalid));
    assert!(!root.join("profile-bindings.json").exists());
    bind(&mut gate, &plan, 7).unwrap();
    let before = fs::read(root.join("profile-bindings.json")).unwrap();
    let mut restarted = ProfileAdmission::load(&root.join("profile-bindings.json"));
    assert_eq!(bind(&mut restarted, &plan, 8), Err(E::ProfilePlanInvalid));
    assert_eq!(
        fs::read(root.join("profile-bindings.json")).unwrap(),
        before
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cas_uses_disk_authority_and_allocates_revisions_inside_the_transaction() {
    let (mut first, root) = fixture();
    let mut other = ProfileAdmission::load(&root.join("profile-bindings.json"));
    let a = first.plan_bind(7, request("binding-a")).unwrap();
    let b = other.plan_bind(7, request("binding-b")).unwrap();
    bind(&mut first, &a, 7).unwrap();
    assert_eq!(bind(&mut other, &b, 7), Err(E::ProfileRevisionConflict));
    assert_eq!(other.status(true)["policy_revision"], 1);
    let b = other.plan_bind(7, request("binding-a")).unwrap();
    assert_eq!(b["review"]["expected_revision"], 1);
    assert_eq!(b["review"]["binding"]["revision"], 2);
    assert_eq!(bind(&mut other, &b, 7).unwrap()["binding"]["revision"], 2);
    assert_eq!(
        floor_revision(&root.join("profile-bindings.json")).unwrap(),
        Some(2)
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unknown_receipts_survive_and_conflicting_receipts_refuse() {
    let (mut gate, root) = fixture();
    let plan = gate.plan_bind(7, request("binding-a")).unwrap();
    let applied = bind(&mut gate, &plan, 7).unwrap();
    let path = root.join("profile-bindings.json");
    let mut raw: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let future = json!({"operation_id":"future-operation","new_schema":true});
    raw["operations"]
        .as_array_mut()
        .unwrap()
        .push(future.clone());
    fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
    let mut restarted = ProfileAdmission::load(&path);
    let next = restarted.plan_bind(7, request("binding-a")).unwrap();
    bind(&mut restarted, &next, 7).unwrap();
    let mut saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["operations"][1], future);
    assert_eq!(bind(&mut restarted, &plan, 7).unwrap(), applied);
    saved["operations"]
        .as_array_mut()
        .unwrap()
        .push(json!({"operation_id":plan["plan"],"new_schema":true}));
    fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    restarted = ProfileAdmission::load(&path);
    assert_eq!(
        bind(&mut restarted, &plan, 7),
        Err(E::PolicyRevisionUnavailable)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
        saved
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn mesh_ceiling_defaults_read_only_and_operator_review_persists_operate() {
    let (mut gate, root) = fixture();
    let plan = gate.plan_bind(7, request("binding-read")).unwrap();
    assert_eq!(plan["review"]["binding"]["mesh_scopes"], json!(["read"]));
    bind(&mut gate, &plan, 7).unwrap();
    let mut raw = serde_json::to_value(request("binding-operate")).unwrap();
    raw["mesh_scopes"] = json!(["read", "operate"]);
    let plan = gate
        .plan_bind(7, serde_json::from_value(raw).unwrap())
        .unwrap();
    bind(&mut gate, &plan, 7).unwrap();
    let loaded = ProfileAdmission::load(&root.join("profile-bindings.json"));
    let status = loaded.status(true);
    let binding = status["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["binding_id"] == "binding-operate")
        .unwrap();
    assert_eq!(binding["mesh_scopes"], json!(["read", "operate"]));
    fs::remove_dir_all(root).unwrap();
}
