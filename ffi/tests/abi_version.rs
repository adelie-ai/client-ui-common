//! Spec for the ABI version symbol (#86).
//!
//! A consumer that compiled against one header and loaded a later library
//! gets no error today. This constant exists so a consumer can compare it for
//! exact equality against the value it compiled with and refuse to run on a
//! mismatch. These tests hold the contract: the header and the runtime value
//! never drift apart, and a cbindgen drop of the constant is loud rather than
//! silent.

use adele_client_core::{ADELE_CORE_ABI_VERSION, adele_core_abi_version};

/// The generated header the C++ consumer includes, read at compile time.
const HEADER: &str = include_str!("../include/adele_client_core.h");

#[test]
fn abi_version_is_never_zero_so_it_cannot_read_as_uninitialized() {
    // `0` is the property that has to hold forever, not any specific value:
    // a C caller that forgets to check the return and reads a zeroed struct
    // must not mistake that for a real version. The starting value of `1` is
    // covered by `header_constant_matches_runtime_value` reading the header
    // as committed today, not pinned as a literal here.
    assert_ne!(adele_core_abi_version(), 0);
}

#[test]
fn runtime_function_returns_rust_constant_not_hardcoded_literal() {
    // Compares against the named constant, not a literal, so a change to
    // `ADELE_CORE_ABI_VERSION` cannot silently leave the function behind.
    assert_eq!(adele_core_abi_version(), ADELE_CORE_ABI_VERSION);
}

#[test]
fn abi_version_constant_present_in_generated_header() {
    // cbindgen silently drops constants it cannot express as a C literal
    // (two `pub const &str` items in this crate already are). A `u32` has no
    // such problem, but this is the only local signal that would catch it if
    // generation ever failed to emit this one.
    assert!(
        HEADER.contains("ADELE_CORE_ABI_VERSION"),
        "generated header is missing ADELE_CORE_ABI_VERSION; cbindgen may have \
         failed or dropped the constant"
    );
}

#[test]
fn header_constant_matches_runtime_value() {
    let define_line = HEADER
        .lines()
        .find(|line| line.starts_with("#define ADELE_CORE_ABI_VERSION"))
        .expect("header must carry a #define for ADELE_CORE_ABI_VERSION");
    let header_value: u32 = define_line
        .rsplit(' ')
        .next()
        .expect("#define line has a value token")
        .parse()
        .expect("ADELE_CORE_ABI_VERSION value parses as u32");
    assert_eq!(header_value, adele_core_abi_version());
}
