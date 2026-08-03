//! PreVocal standalone binary entry point.
//!
//! All desktop logic (audio I/O, device selection, GUI wiring) lives in
//! [`standalone`], so this file stays a thin launcher. The plugin itself is
//! implemented in `lib.rs` and needs no standalone path.

#[cfg(feature = "standalone")]
mod standalone;

fn main() {
    #[cfg(feature = "standalone")]
    standalone::run();

    #[cfg(not(feature = "standalone"))]
    {
        eprintln!("Standalone mode is disabled. Build with `cargo run --features standalone --bin prevocal-standalone`.");
    }
}
