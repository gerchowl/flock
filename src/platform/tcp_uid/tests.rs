use super::*;
#[test]
fn real_loopback_tuple_identifies_client_uid_and_never_the_listener_by_port_alone() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (accepted, peer) = listener.accept().unwrap();
    let local = accepted.local_addr().unwrap();
    assert_eq!(
        lookup(peer, local),
        Some(crate::platform::peer::effective_uid())
    );
    assert_eq!(lookup(peer, SocketAddr::new(local.ip(), 0)), None);
    drop(client);
}
#[test]
fn kernel_record_parsers_require_exact_tuple_and_socket_pairing() {
    let peer = "127.0.0.1:12345".parse().unwrap();
    let local = "127.0.0.1:54321".parse().unwrap();
    let mut data = vec![0u8; 24 + 104 + 104 + 24];
    for (offset, len) in [(0, 24u32), (24, 104), (128, 104), (232, 24)] {
        data[offset..offset + 4].copy_from_slice(&len.to_ne_bytes());
    }
    data[28..32].copy_from_slice(&0x10u32.to_ne_bytes());
    data[32..40].copy_from_slice(&123u64.to_ne_bytes());
    data[40..42].copy_from_slice(&54321u16.to_be_bytes());
    data[42..44].copy_from_slice(&12345u16.to_be_bytes());
    data[68] = 1;
    data[84..88].copy_from_slice(&[127, 0, 0, 1]);
    data[100..104].copy_from_slice(&[127, 0, 0, 1]);
    data[132..136].copy_from_slice(&1u32.to_ne_bytes());
    data[156..164].copy_from_slice(&123u64.to_ne_bytes());
    data[192..196].copy_from_slice(&1234u32.to_ne_bytes());
    assert_eq!(mac_records(&data, peer, local), Some(1234));
    data[156] ^= 1;
    assert_eq!(mac_records(&data, peer, local), None);
    assert_eq!(mac_records(&data[..30], peer, local), None);
    let row =
        "0: 0100007F:3039 0100007F:D431 01 00000000:00000000 00:00000000 00000000 1234 0 12345";
    assert_eq!(linux_records(row, peer, local), Some(1234));
    assert_eq!(linux_records(row, local, peer), None);
    assert_eq!(
        linux_records(&row.replace(" 01 ", " 0A "), peer, local),
        None
    );
}
