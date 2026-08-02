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

O standalone usa o **dispositivo de entrada (microfone) e de saída padrão do sistema**,
funcionando no Windows (WASAPI) e no Linux (ALSA/JACK via `cpal`).

A barra **AUDIO** no topo da janela permite:

- **Selecionar o driver/host e o dispositivo** (ex.: WASAPI, ASIO, ALSA) por um menu.
  Se o dispositivo escolhido só tiver entrada (ou só saída), o outro lado cai no
  padrão do host.
- **↻** reenumerar a lista de dispositivos (ex.: depois de plugar uma interface).
- **RESTART** reiniciar o motor de áudio com o dispositivo selecionado (aplica a troca
  e recupera de erros).

A última seleção fica salva em `prevocal-last-device.txt` (ao lado do executável) e é
restaurada no próximo launch. O status à direita indica `RUNNING` (verde), erro
(vermelho) ou vazio (cinza).

### ASIO no Windows (baixa latência)

Para usar drivers ASIO (ASIO4ALL, interfaces com driver nativo), habilite a feature:

```sh
cargo run --features standalone,asio --bin prevocal-standalone
```

Pré-requisitos para compilar o host ASIO do `cpal`:

1. **Visual Studio Build Tools** com o workload C++ (MSVC + Windows SDK).
2. **LLVM/Clang** no `PATH` (o cpal usa clang para compilar o SDK da Steinberg).

> Nota: a feature `asio` é opcional e só é compilada quando ativada. Se você não
> precisar de ASIO, mantenha o comando padrão (`--features standalone`).

Prerequisitos (Linux): `libasound2-dev` (ALSA) ou JACK para áudio via `cpal`.

O fluxo de áudio é:

```text
dispositivo selecionado (entrada) -> DSP (Drive -> HPF -> Air -> Trim -> Phase) -> dispositivo selecionado (saída)
          |                                                                      |
          +--> meter IN (sinal bruto do mic)                                      +--> meter OUT (sinal processado)
```

> Nota: se você não ouvir nada, confira se o microfone padrão do sistema está
> captando (Windows: Configurações > Sistema > Som > Entrada; Linux: `pavucontrol`).

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
