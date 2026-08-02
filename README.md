# PreVocal

PreVocal is a vocal bus preamp with standalone Slint GUI and plugin targets via nice-plug.

## Run standalone

cargo run --bin prevocal-standalone

## Build plugin bundles

cargo build --release

# Notes
- Standalone uses nice-plug standalone mode.
- Plugin library exports VST3 and CLAP.
