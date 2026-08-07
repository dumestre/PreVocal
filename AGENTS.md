# PreVocal — Developer Notes

## Build bundles (VST3 + CLAP)

```powershell
cargo nice-plug bundle PreVocal --release
```

Output goes to `target/bundled/`:

- `target/bundled/PreVocal.vst3\Contents\x86_64-win\PreVocal.vst3` (bundle folder)
- `target/bundled/PreVocal.clap`

## Install into the DAW VST folder

The Windows VST2/VST3/CLAP search folder is `C:\Program Files\Common Files\VST3`.
The installed files are **flat** (not folder bundles), and the folder requires
elevation to write to, so copy with an elevated PowerShell:

```powershell
Start-Process powershell -Verb RunAs -Wait -ArgumentList '-NoProfile','-Command',"Copy-Item -LiteralPath 'D:\Dev\Porjetos\PreVocal\target\bundled\PreVocal.vst3\Contents\x86_64-win\PreVocal.vst3' -Destination 'C:\Program Files\Common Files\VST3\PreVocal.vst3' -Force; Copy-Item -LiteralPath 'D:\Dev\Porjetos\PreVocal\target\bundled\PreVocal.clap' -Destination 'C:\Program Files\Common Files\VST3\PreVocal.clap' -Force"
```

After copying, verify timestamps:

```powershell
Get-Item "C:\Program Files\Common Files\VST3\PreVocal.vst3","C:\Program Files\Common Files\VST3\PreVocal.clap" | Select-Object FullName, Length, LastWriteTime
```

## Logs

- `C:\temp\prevocal_plugin.log` — plugin trace log (TRACE level, appends; delete before a clean test).
- `C:\temp\prevocal_load.log` — plugin load log (rewritten per instance).

## Important

- **Do NOT edit cargo git checkouts** of dependencies (e.g. `C:\Users\DJDUM\.cargo\git\checkouts\nice-plug-*`): cargo re-restores them on every build. Patched crates live in `vendor/nice-plug` and are wired via `Cargo.toml` path dependencies.
