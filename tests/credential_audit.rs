//! Dedicated binary: the allocation audit never replaces the product unit allocator.
use base64::Engine;
use std::ffi::OsString;
#[path = "../src/structured/lease/credential.rs"]
mod credential;
mod structured {
    pub(crate) mod lease {
        pub(crate) use crate::credential::LeaseCredential;
    }
}
#[path = "../src/mcp/client/config.rs"]
mod config;
#[path = "../src/api/secret_json.rs"]
mod secret_json;

#[global_allocator]
static ALLOCATOR: SecretAudit = SecretAudit;
struct SecretAudit;
thread_local! {
    static AUDIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static LEAKED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
// SAFETY: System owns every allocation. Zero-initializing capacity also makes
// inspection of unused bytes valid. Inspection precedes deallocation and never
// allocates itself. This allocator exists only in the test binary.
unsafe impl std::alloc::GlobalAlloc for SecretAudit {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        unsafe { std::alloc::GlobalAlloc::alloc_zeroed(&std::alloc::System, layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        // The audited startup path's matching allocations are encoded UTF-8
        // byte buffers. Exclude object layouts and inspect only their live bytes.
        if layout.size() >= 8
            && layout.align() == 1
            && AUDIT.try_with(|active| active.get()).unwrap_or(false)
        {
            // SAFETY: the allocation is still live and contains at least eight bytes.
            if unsafe { std::slice::from_raw_parts(ptr, layout.size()) }
                .windows(8)
                .any(|bytes| bytes == b"KioqKioq")
            {
                let _ = LEAKED.try_with(|count| count.set(count.get() + 1));
            }
        }
        unsafe { std::alloc::GlobalAlloc::dealloc(&std::alloc::System, ptr, layout) }
    }
}
#[test]
fn startup_credential_buffers_are_zeroized_before_deallocation() {
    let secret = base64::engine::general_purpose::STANDARD.encode([42; 32]);
    // Prove the audit catches an ordinary unzeroized buffer before relying on
    // its zero count for the startup path. Black-box ownership keeps it live.
    let control = OsString::from(&secret);
    LEAKED.set(0);
    AUDIT.set(true);
    drop(std::hint::black_box(control));
    AUDIT.set(false);
    assert_eq!(LEAKED.get(), 1, "audit failed to detect its control buffer");
    for structured in [true, false] {
        let raw = zeroize::Zeroizing::new(secret.as_bytes().to_vec());
        let socket = OsString::from("/tmp/fixture.sock");
        LEAKED.set(0);
        AUDIT.set(true);
        let result = std::hint::black_box(config::parse(structured, Some(raw), Some(socket)));
        assert_eq!(result.is_ok(), structured);
        if let Ok(config::Transport::Scoped(path, credential)) = &result {
            assert!(path.is_absolute());
            assert!(!format!("{credential:?}").contains(&secret));
        }
        drop(result);
        AUDIT.set(false);
        assert_eq!(LEAKED.get(), 0, "credential bytes freed before zeroization");
    }
}

#[test]
fn credential_generation_and_matching_use_the_real_implementation() {
    let credential = credential::LeaseCredential::generate().unwrap();
    assert!(credential.matches(&credential.clone()));
}

#[test]
fn envelope_encoding_and_parsed_credentials_are_scrubbed_on_success_and_refusal() {
    let secret = base64::engine::general_purpose::STANDARD.encode([42; 32]);
    let value = serde_json::json!({"credential":secret,"tail":"x".repeat(1024)});
    let line = serde_json::to_string(&value).unwrap();
    LEAKED.set(0);
    AUDIT.set(true);
    let wire = secret_json::encode(&value).unwrap();
    assert_eq!(&*wire, line.as_bytes());
    drop(wire);
    let mut input = secret_json::Buffer::default();
    for byte in line.bytes() {
        std::io::Write::write_all(&mut input, &[byte]).unwrap();
    }
    let text = zeroize::Zeroizing::new(std::str::from_utf8(&input.0).unwrap().to_owned());
    drop(text);
    drop(input);
    let parsed = secret_json::SecretValue::parse(&line).unwrap();
    assert!(<u64 as serde::Deserialize>::deserialize(&parsed.0).is_err());
    drop(parsed);
    AUDIT.set(false);
    assert_eq!(LEAKED.get(), 0, "credential JSON freed before scrubbing");
}

#[test]
fn escaped_credential_is_refused_before_serde_scratch_copies_it() {
    let secret = base64::engine::general_purpose::STANDARD.encode([42; 32]);
    let escaped = format!(r#""\u004b{}""#, &secret[1..]);
    for value in [
        escaped.clone(),
        format!("[{escaped}]"),
        format!(r#"{{"nested":{escaped}}}"#),
    ] {
        let line = format!(r#"{{"credential":{value}}}"#);
        LEAKED.set(0);
        AUDIT.set(true);
        let refused = secret_json::SecretValue::parse(&line).is_err();
        AUDIT.set(false);
        assert_eq!(
            (refused, LEAKED.get()),
            (true, 0),
            "credential must be refused without an unzeroized scratch copy"
        );
    }
}
