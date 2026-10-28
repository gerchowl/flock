use super::*;

#[test]
fn exit_after_kernel_arm_before_identity_check_is_refused() {
    let mut peer = Peer::spawn();
    let accepted = peer.credentials;
    let witness = ExitWatch::arm(accepted).unwrap();
    let result = ExitWatch::checked(accepted, |_| {
        peer.exit();
        assert!(readable(witness.as_raw_fd(), Duration::from_secs(5)));
        assert!(peer.child.wait().unwrap().success());
        accepted.birth
    });
    assert!(
        result.is_err(),
        "cached birth must not authorize an exited peer"
    );
}
