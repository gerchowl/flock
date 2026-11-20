use super::super::bindings::transactions::{BindingSpec, PlanRequest, RevokeRequest};
use super::*;
use std::os::unix::fs::{symlink, PermissionsExt};

const A: &str = "11111111-1111-4111-8111-111111111111";
const B: &str = "22222222-2222-4222-8222-222222222222";

struct Fixture {
    dir: crate::structured::launch::SessionConfigDir,
    policy: PathBuf,
    gate: ProfileAdmission,
    pin: BindingIdentity,
}
impl Fixture {
    fn new() -> Self {
        let dir =
            crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let policy = dir.path().join("profile-bindings.json");
        let mut gate = ProfileAdmission::load(&policy);
        let spec = BindingSpec {
            binding_id: None,
            target: super::super::bindings::Scope {
                repo_id: "fixture-repo".into(),
                node_ids: vec!["node.example".into()],
                agent_ids: vec!["fixture-a".into(), "fixture-b".into()],
            },
            account_alias: "fixture-account".into(),
            credential_ref: "unopened-reference".into(),
            credential_generation: 1,
            bundle_digests: vec!["fixture-digest".into()],
            grants: vec![],
        };
        let plan = gate.plan_bind(7, PlanRequest::Bind(spec)).unwrap();
        gate.bind(
            7,
            plan["plan"].as_str().unwrap(),
            plan["expect"].as_str().unwrap(),
        )
        .unwrap();
        let binding = gate
            .store
            .resolve("fixture-repo", "node.example", "fixture-a")
            .unwrap();
        let pin = BindingIdentity {
            binding_id: binding.binding_id,
            revision: binding.revision,
        };
        Self {
            dir,
            policy,
            gate,
            pin,
        }
    }
    fn allocate(&mut self, session: &str, agent: &str) -> Result<Value, E> {
        self.gate
            .allocate_session(session, "fixture-repo", "node.example", agent, &self.pin)
    }
    fn root(&self, session: &str) -> PathBuf {
        base(&self.policy).unwrap().join("sessions").join(session)
    }
    fn record(&self, session: &str, agent: &str) -> Allocation {
        Allocation {
            schema: 1,
            session_id: session.into(),
            repo_id: "fixture-repo".into(),
            node_id: "node.example".into(),
            agent_id: agent.into(),
            binding: self.pin.clone(),
        }
    }
}

#[test]
fn same_account_sessions_are_private_durable_and_independent() {
    let mut f = Fixture::new();
    let a = f.allocate(A, "fixture-a").unwrap();
    let b = f.allocate(B, "fixture-b").unwrap();
    assert_ne!(a["native_root"], b["native_root"]);
    let native = PathBuf::from(a["native_root"].as_str().unwrap());
    fs::write(native.join("transcript"), b"private evidence").unwrap();
    assert!(!Path::new(b["native_root"].as_str().unwrap())
        .join("transcript")
        .exists());
    for session in [A, B] {
        for path in [
            base(&f.policy).unwrap(),
            base(&f.policy).unwrap().join("sessions"),
            f.root(session),
            f.root(session).join("config"),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
    f.gate = ProfileAdmission::load(&f.policy);
    assert_eq!(f.allocate(A, "fixture-a").unwrap(), a);
    assert_eq!(
        fs::read(native.join("transcript")).unwrap(),
        b"private evidence"
    );
    assert_eq!(
        f.gate.status(true)["session_storage"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(f.gate.status(false), json!({}));
    assert_eq!(read(&f.root(A)).unwrap().binding, f.pin);
}

#[test]
fn failed_allocation_preserves_prior_roots_and_removes_only_its_own_staging() {
    let mut f = Fixture::new();
    f.allocate(A, "fixture-a").unwrap();
    let original = fs::read(f.root(A).join("record.json")).unwrap();
    let b = f.record(B, "fixture-b");
    assert!(allocate_with(&f.policy, &b, || Err(io::Error::other(
        "injected before publication"
    )))
    .is_err());
    assert!(!f.root(B).exists());
    let staging = f.root(B).with_file_name(format!(".alloc-{B}"));
    assert!(!staging.exists());
    crate::platform::structured::create_private_dir(&staging).unwrap();
    fs::write(staging.join("evidence"), b"earlier call").unwrap();
    assert!(allocate(&f.policy, &b).is_err());
    assert!(f.allocate(B, "fixture-b").is_err());
    assert_eq!(fs::read(staging.join("evidence")).unwrap(), b"earlier call");
    assert_eq!(fs::read(f.root(A).join("record.json")).unwrap(), original);
    assert_eq!(read(&f.root(A)).unwrap().session_id, A);
}

#[test]
fn allocation_cannot_enable_execution_or_access_a_credential() {
    let mut f = Fixture::new();
    // This reference is deliberately not a usable credential path.
    f.allocate(A, "fixture-a").unwrap();
    assert!(!super::super::production_ready());
    assert_eq!(f.gate.enable(), Err(super::super::ProfileRefusal::NotReady));
    assert_eq!(f.gate.admit(), Err(super::super::ProfileRefusal::Disabled));
    assert_eq!(f.gate.status(true)["execution_switch"], false);
    assert!(f.dir.path().read_dir().unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains("credential")));
}

#[test]
fn startup_quarantines_only_unpublished_staging_and_reports_it_after_restart() {
    let mut f = Fixture::new();
    f.allocate(A, "fixture-a").unwrap();
    let staging = f.root(B).with_file_name(format!(".alloc-{B}"));
    crate::platform::structured::create_private_dir(&staging).unwrap();
    fs::write(staging.join("partial"), b"crash evidence").unwrap();
    for _ in 0..2 {
        f.gate = ProfileAdmission::load(&f.policy);
        let status = f.gate.status(true);
        assert_eq!(
            status["session_storage"]["quarantined"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            status["session_storage"]["sessions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(f.root(A).join("record.json").is_file());
    }
    assert!(!staging.exists());
    let quarantine = base(&f.policy).unwrap().join("quarantine");
    let entry = fs::read_dir(quarantine).unwrap().next().unwrap().unwrap();
    assert_eq!(
        fs::read(entry.path().join("partial")).unwrap(),
        b"crash evidence"
    );
}

#[test]
fn unknown_invalid_records_and_bad_roots_remain_raw_and_inactive() {
    let mut f = Fixture::new();
    f.allocate(A, "fixture-a").unwrap();
    f.allocate(B, "fixture-b").unwrap();
    let path = f.root(B).join("record.json");
    for raw in [
        json!({"schema":42,"future":true}),
        json!({"schema":1,"session_id":B}),
        json!(null),
    ] {
        let bytes = serde_json::to_vec(&raw).unwrap();
        fs::write(&path, &bytes).unwrap();
        f.gate = ProfileAdmission::load(&f.policy);
        let status = f.gate.status(true);
        assert_eq!(status["session_storage"]["retained_unknown"], 1);
        assert_eq!(
            status["session_storage"]["sessions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(f.allocate(B, "fixture-b").is_err());
    }
}

#[test]
fn target_pin_revision_revocation_and_path_escapes_refuse_before_allocation() {
    let mut f = Fixture::new();
    assert!(f.allocate("../escape", "fixture-a").is_err());
    assert!(f.allocate(A, "unbound").is_err());
    f.pin.revision += 1;
    assert_eq!(
        f.allocate(A, "fixture-a"),
        Err(E::StructuredProfileMismatch)
    );
    f.pin.revision -= 1;
    assert!(!f.root(A).exists());
    f.allocate(A, "fixture-a").unwrap();
    assert!(f.allocate(A, "fixture-b").is_err());
    f.gate
        .revoke(
            7,
            RevokeRequest {
                binding: f.pin.binding_id.clone(),
                reason: "fixture revocation".into(),
            },
        )
        .unwrap();
    assert!(f.allocate(B, "fixture-b").is_err());
    assert!(f.root(A).is_dir());
    assert!(!f.root(B).exists());
}

#[test]
fn symlinks_and_nonprivate_directories_never_supply_native_roots() {
    let mut f = Fixture::new();
    f.allocate(A, "fixture-a").unwrap();
    let external =
        crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    symlink(external.path(), f.root(B)).unwrap();
    assert!(f.allocate(B, "fixture-b").is_err());
    assert_eq!(external.path().read_dir().unwrap().count(), 0);
    fs::set_permissions(f.root(A).join("config"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(f.allocate(A, "fixture-a").is_err());
    f.gate = ProfileAdmission::load(&f.policy);
    assert_eq!(
        f.gate.status(true)["session_storage"]["retained_unknown"],
        2
    );
}
