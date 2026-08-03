//! PreVocal standalone binary entry point.
//!
//! All desktop logic (audio I/O, device selection, GUI wiring) lives in
//! [`standalone`], so this file stays a thin launcher. The plugin itself is
//! implemented in `lib.rs` and needs no standalone path.

mod standalone;

fn main() {
    standalone::run();
}
