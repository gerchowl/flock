use super::*;
#[test]
fn near_global_cap_hot_operations_write_only_one_record() {
    let root = root();
    let journal = Journal::open(root.path()).unwrap();
    let mut file = io::BufWriter::new(journal.file.try_clone().unwrap());
    for n in 0..GLOBAL_LIMIT - 1 {
        let record = Record {
            version: 1,
            namespace: "local".into(),
            principal: format!("p{}", n / PRINCIPAL_LIMIT),
            cmd_id: format!("id{n}"),
            digest: [0; 32],
            admitted_ms: 100,
            outcome: None,
        };
        serde_json::to_writer(&mut file, &record).unwrap();
        file.write_all(b"\n").unwrap();
    }
    file.flush().unwrap();
    drop(file);
    drop(journal);
    let start = std::time::Instant::now();
    let mut journal = Journal::open(root.path()).unwrap();
    eprintln!(
        "capacity startup replay {:?}; entries {}",
        start.elapsed(),
        journal.index.records.len()
    );
    let claim = |id: &str, now| Operation::Claim {
        namespace: "local".into(),
        principal: "last".into(),
        command: command(id),
        now,
    };
    let written = journal.file.written;
    let start = std::time::Instant::now();
    assert!(matches!(
        journal.apply(claim("last", 101)).unwrap(),
        Admission::Claimed(101)
    ));
    let claim_bytes = journal.file.written - written;
    eprintln!(
        "capacity hot claim {:?}; bytes {claim_bytes}",
        start.elapsed()
    );
    let start = std::time::Instant::now();
    let Admission::Replayed(first) = journal
        .apply(Operation::Finish {
            key: ("local".into(), "last".into(), "last".into()),
            admitted_ms: 101,
            result: Ok(json!({"ok":true})),
        })
        .unwrap()
    else {
        panic!("finish did not return its durable outcome");
    };
    let finish_bytes = journal.file.written - written - claim_bytes;
    eprintln!(
        "capacity hot finish {:?}; bytes {finish_bytes}",
        start.elapsed()
    );
    assert!(claim_bytes <= ENTRY_BYTES as u64);
    assert!(finish_bytes <= ENTRY_BYTES as u64);
    let start = std::time::Instant::now();
    assert!(
        matches!(journal.apply(claim("last", 102)).unwrap(), Admission::Replayed(frame) if frame == first)
    );
    eprintln!("capacity hot replay {:?}", start.elapsed());
    assert_eq!(journal.file.written, written + claim_bytes + finish_bytes);
    assert_eq!(journal.index.records.len(), GLOBAL_LIMIT);
    assert!(matches!(
        journal.apply(claim("overflow", 102)),
        Err(ErrorCode::SessionBusy)
    ));
}
