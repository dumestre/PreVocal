# PreVocal

Vocal Bus Preamp escrito em Rust com DSP de tempo real, interface Slint e
suporte a plugins VST3/CLAP via [nice-plug](https://codeberg.org/RustAudio/nice-plug).

## Recursos DSP

| Controle      | Faixa         | Descrição                              |
| ------------- | ------------- | -------------------------------------- |
| Drive         | 0 – 24 dB     | Saturação macia com `tanh`             |
| HPF           | 20 – 200 Hz   | Filtro passa-altas Butterworth 2ª ordem|
| Air           | 0 – 6 dB      | High-shelf a 10 kHz                    |
| Phase Flip    | on/off        | Inversão de polaridade (180°)          |
| Output Trim   | –12 – +12 dB  | Ganho de saída                         |

## Executar no modo Standalone

```sh
cargo run --features standalone --bin prevocal-standalone
```

Prerequisitos (Linux): `libasound2-dev` (ALSA) ou JACK para áudio via `cpal`.

## Gerar bundles de plugin (VST3 / CLAP)

Instale a ferramenta de empacotamento:

```sh
cargo install cargo-nice-plug
```

Compile e empacote:

```sh
cargo build --release
cargo nice-plug bundle --release
```

Os bundles aparecerão em `target/nice-plug/`.
