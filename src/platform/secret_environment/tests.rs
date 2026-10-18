use super::*;
use std::{cell::Cell, ffi::CString};

#[test]
fn duplicate_entries_are_all_scrubbed_before_removal_and_first_value_is_captured() {
    let name = c"FLOCK_MCP_LEASE_CREDENTIAL";
    let values = ["first-secret", "second-secret-is-longer", "third"];
    let mut storage: Vec<Vec<u8>> = values
        .iter()
        .map(|value| {
            CString::new(format!("{}={value}", name.to_str().unwrap()))
                .unwrap()
                .into_bytes_with_nul()
        })
        .collect();
    let untouched = CString::new("FLOCK_MCP_LEASE_CREDENTIAL_SUFFIX=not-the-key")
        .unwrap()
        .into_bytes_with_nul();
    storage.insert(1, untouched.clone());
    let mut entries: Vec<*mut libc::c_char> = storage
        .iter_mut()
        .map(|bytes| bytes.as_mut_ptr().cast())
        .collect();
    entries.push(std::ptr::null_mut());
    // SAFETY: the first owned string contains the full key, equals sign and value.
    let first = unsafe { entries[0].add(name.to_bytes().len() + 1) };
    let removed = Cell::new(false);
    // SAFETY: the constructed table and all writable C strings are private to
    // this test; neither the process environment nor another thread sees them.
    let captured = unsafe {
        take_from(name, entries.as_mut_ptr(), first, || {
            for (index, expected) in [(0, values[0]), (2, values[1]), (3, values[2])] {
                let bytes = CStr::from_ptr(entries[index]).to_bytes();
                assert_eq!(
                    &bytes[name.to_bytes().len() + 1..],
                    vec![b'0'; expected.len()],
                    "duplicate value at index {index} was not fully scrubbed before removal"
                );
                assert_eq!(storage[index].last(), Some(&0));
            }
            assert_eq!(storage[1], untouched, "a longer key must not be scrubbed");
            removed.set(true);
            Ok(())
        })
    }
    .unwrap()
    .unwrap();
    assert!(removed.get());
    assert_eq!(captured.as_slice(), values[0].as_bytes());
}
