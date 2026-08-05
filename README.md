# PreVocal

Vocal Bus Preamp escrito em Rust com DSP de tempo real, interface Slint e
suporte a plugins VST3/CLAP via [nice-plug](https://codeberg.org/RustAudio/nice-plug).

## Recursos DSP

| Controle      | Faixa          | Descrição                                      |
| ------------- | -------------- | ---------------------------------------------- |
| Drive         | 0 – 24 dB      | Saturação macia com `tanh`                     |
| HPF           | 20 – 200 Hz    | Filtro passa-altas Butterworth 2ª ordem        |
| LPF           | 500 – 20 kHz   | Filtro passa-baixas Butterworth 2ª ordem       |
| Air           | 0 – 6 dB       | High-shelf a 10 kHz                            |
| Compressor    | -60 – 0 dB     | Threshold, Ratio, Attack, Release, Makeup      |
| Delay         | 1 – 1000 ms    | Delay estéreo (eco), sinal principal mono      |
| Output Trim   | –12 – +12 dB   | Ganho de saída                                 |

Cadeia do sinal:

```text
Drive -> HPF -> LPF -> Air -> Compressor -> Trim -> Delay (estéreo)
```

## Guia rápido de comandos

### 1) Rodar o standalone (testar com microfone, WASAPI — padrão Windows)

```sh
cargo run --bin prevocal-standalone
```

### 2) Rodar o standalone com ASIO (baixa latência, interface de áudio)

Na primeira vez, aponte o LLVM/Clang (obrigatório para compilar o SDK ASIO):

```sh
$env:LIBCLANG_PATH = "$env:ProgramFiles\LLVM\bin"
$env:PATH = "$env:ProgramFiles\LLVM\bin;$env:PATH"
cargo run --features asio --bin prevocal-standalone
```

Pré-requisitos: **Visual Studio Build Tools** (workload C++, MSVC + Windows SDK) e **LLVM/Clang** instalado.

### 3) Gerar os plugins VST3/CLAP (para usar na DAW)

```sh
cargo install cargo-nice-plug
$env:LIBCLANG_PATH = "$env:ProgramFiles\LLVM\bin"
$env:PATH = "$env:ProgramFiles\LLVM\bin;$env:PATH"
cargo nice-plug bundle PreVocal --release
```

Os bundles aparecem em `target/nice-plug/`.

---

## Usando o standalone

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

> Se você não ouvir nada, confira se o microfone padrão do sistema está captando
> (Windows: Configurações > Sistema > Som > Entrada; Linux: `pavucontrol`).
>
> Pré-requisitos (Linux): `libasound2-dev` (ALSA) ou JACK para áudio via `cpal`.
