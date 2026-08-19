//! The ELF parser treats its input as untrusted binary data.
//!
//! These tests are the always-on complement to the `cargo fuzz` target in
//! `fuzz/`: they run in a normal `cargo test` and are deterministic, so a
//! regression that makes the parser panic fails the build rather than waiting
//! for someone to start a fuzzing session.

use std::path::Path;

use elfpak_core::ElfMetadata;

/// Deterministic pseudo-random source, so a failure is always reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // SplitMix64.
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

/// A small, valid ELF to mutate. Size matters: these tests parse it thousands
/// of times, and the test binary itself is megabytes of debug info.
fn sample() -> Vec<u8> {
    let candidates = ["/usr/bin/true", "/bin/true", "/usr/bin/echo"];
    for candidate in candidates {
        if let Ok(bytes) = std::fs::read(candidate)
            && ElfMetadata::looks_like_elf(&bytes)
        {
            return bytes;
        }
    }
    let exe = std::env::current_exe().expect("test binary path");
    std::fs::read(exe).expect("test binary is readable")
}

/// Parsing must return a value or an error, never panic and never hang.
fn parse(bytes: &[u8]) {
    let _ = ElfMetadata::parse_bytes(Path::new("<fuzz>"), bytes);
}

#[test]
fn truncated_inputs_never_panic() {
    let bytes = sample();
    // Dense coverage of the headers, sparse coverage of the rest.
    for len in 0..512.min(bytes.len()) {
        parse(&bytes[..len]);
    }
    let mut len = 512;
    while len < bytes.len() {
        parse(&bytes[..len]);
        len += 997;
    }
}

#[test]
fn single_byte_corruption_never_panics() {
    let bytes = sample();
    let mut rng = Rng(0x1234_5678);
    for _ in 0..2_000 {
        let mut mutated = bytes.clone();
        // The first kilobyte holds the headers most parsing decisions rely on.
        let offset = if rng.next().is_multiple_of(2) {
            rng.below(1024.min(mutated.len()))
        } else {
            rng.below(mutated.len())
        };
        mutated[offset] ^= 1 << (rng.below(8));
        parse(&mutated);
    }
}

#[test]
fn structured_garbage_never_panics() {
    let mut rng = Rng(0xdead_beef);
    for size in [4, 16, 64, 128, 256, 1024, 4096] {
        for _ in 0..64 {
            let mut bytes = vec![0u8; size];
            for byte in bytes.iter_mut() {
                *byte = rng.next() as u8;
            }
            // Half the samples look like ELF so the parser gets past the magic.
            if rng.next().is_multiple_of(2) && bytes.len() >= 4 {
                bytes[..4].copy_from_slice(b"\x7fELF");
            }
            parse(&bytes);
        }
    }
}

#[test]
fn header_fields_can_claim_anything() {
    let bytes = sample();
    if bytes.len() < 64 {
        return;
    }
    // e_ident class/data/version, e_type, e_machine, e_phoff, e_phnum, e_shnum.
    for offset in [4usize, 5, 6, 7, 16, 18, 32, 40, 56, 58, 60] {
        for value in [0u8, 1, 2, 0x7f, 0xff] {
            let mut mutated = bytes.clone();
            mutated[offset] = value;
            parse(&mutated);
            // Also corrupt the adjacent byte, so multi-byte fields go wild.
            if offset + 1 < mutated.len() {
                mutated[offset + 1] = value;
                parse(&mutated);
            }
        }
    }
}
