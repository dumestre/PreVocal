# PreVocal — Vocal Bus Preamp

**PreVocal** é um channel strip completo para vocal, projetado como "bus vocal" — coloca-se no insert do canal de voz (ou bus de vozes) e entrega saturação estilo válvula, equalização cirúrgica, compressão musical, delay estéreo e reverb de sala, tudo em uma única janela lado a lado.

Escrito em **Rust** com DSP de tempo real otimizado, interface **Slint** (tema dark studio) e suporte nativo a **VST3** e **CLAP** via [nice-plug](https://codeberg.org/RustAudio/nice-plug).

---

## Cadeia de sinal (ordem fixa)

```text
PREAMP → COMPRESSOR → DELAY → REVERB → OUTPUT TRIM
```

- **PREAMP** (drive + tone + tube emulation) — saturação primeiro, para que o compressor reaja ao timbre já colorido.
- **COMPRESSOR** — controla dinâmica pós-saturação.
- **DELAY** — eco estéreo (sinal principal permanece mono, taps são estéreo).
- **REVERB** — cauda de sala após o delay; os ecos entram no reverb naturalmente.
- **OUTPUT TRIM** — ganho final.

---

## Seções e controles

### PREAMP
| Controle | Faixa | Descrição |
|----------|-------|-----------|
| **DRIVE** | 0 – 24 dB | Ganho de entrada + saturação `tanh` suave |
| **HPF** | 20 – 200 Hz | Filtro passa-altas Butterworth 12 dB/oct (limpa grave/rumble) |
| **LPF** | 500 – 20 kHz | Filtro passa-baixas Butterworth 12 dB/oct (tira dureza/ess) |
| **AIR** | 0 – 6 dB | High-shelf em 10 kHz (brilho "caro" sem estridência) |
| **CHARACTER** | 0 – 100 % | **Emulação de válvula** — bias assimétrico no estágio de saturação. 0 = simétrico (som limpo/transistor, só harmônicas ímpares). >0 = introduz 2ª harmônica = calor, corpo, "doçura" de tubo (12AX7-style). Default **35 %** para calor leve ao abrir. |
| **SAG** | 0 – 100 % | **Power-supply sag** — envelope follower (ataque instantâneo, release ~80 ms) que reduz o ganho efetivo em transientes fortes. 0 = resposta rígida. >0 = compressão orgânica, "solta", sensação de amp a válvula sob carga. |

### COMPRESSOR
| Controle | Faixa | Descrição |
|----------|-------|-----------|
| **THRESHOLD** | –60 – 0 dB | Limiar de compressão |
| **RATIO** | 1 – 20 :1 | Razão de compressão |
| **ATTACK** | 0.1 – 100 ms | Tempo de ataque (rápido = controla transientes; lento = deixa passar punch) |
| **RELEASE** | 10 – 1000 ms | Tempo de release (rápido = pumping; lento = cola) |
| **MAKEUP** | 0 – 24 dB | Ganho de compensação pós-compressão |
| **BYPASS** | on/off | Desliga o compressor (envelope continua rastreando para não dar "pump" ao reativar) |

### DELAY
| Controle | Faixa | Descrição |
|----------|-------|-----------|
| **TIME** | 1 – 1000 ms | Tempo do delay (tap principal) |
| **FEEDBACK** | 0 – 90 % | Quantidade de repetições |
| **MIX** | 0 – 100 % | Blend dry/wet |
| **BYPASS** | on/off | Desliga delay |

### REVERB
| Controle | Faixa | Descrição |
|----------|-------|-----------|
| **SIZE** | 0 – 1 | Tamanho da sala (comb filter lengths) |
| **DAMPING** | 0 – 1 | Amortecimento HF nas caudas (0 = brilhante, 1 = escuro/absorvente) |
| **MIX** | 0 – 100 % | Blend dry/wet |
| **BYPASS** | on/off | Desliga reverb (default ON para não sujar o sinal acidentalmente) |

### OUTPUT TRIM
| Controle | Faixa | Descrição |
|----------|-------|-----------|
| **TRIM** | –12 – +12 dB | Ganho final de saída (stage gain) |

---

## Presets de fábrica (6)

| Preset | Drive | HPF | LPF | Air | Comp | Delay | Reverb | Character | Sag | Uso sugerido |
|--------|-------|-----|-----|-----|------|-------|--------|-----------|-----|--------------|
| **Default** | 0 dB | 20 Hz | 20 kHz | 0 dB | off | off | off (bypass) | 35 % | 0 % | Ponto de partida neutro |
| **Clean** | 3 dB | 60 Hz | 16 kHz | 1 dB | leve | off | off | 35 % | 0 % | Voz limpa, leve presença |
| **Radio** | 8 dB | 120 Hz | 12 kHz | 2 dB | médio | off | off | 35 % | 0 % | Estilo "locutor de rádio", mid-forward |
| **Punch** | 12 dB | 90 Hz | 14 kHz | 0 dB | forte (8:1) | off | off | 35 % | 0 % | Voz agressiva, rock/pop denso |
| **Echo** | 5 dB | 80 Hz | 16 kHz | 1.5 dB | médio | 350 ms / 45 % | off | 35 % | 0 % | Slapback / delay criativo |
| **Airy** | 6 dB | 100 Hz | 18 kHz | 6 dB | leve | 300 ms / 15 % | **on** (0.7 / 0.3 / 20 %) | 35 % | 0 % | Voz aberta, "arejada", com espaço |

---

## Requisitos & Build

### Windows
- **Visual Studio Build Tools** (workload "Desenvolvimento para desktop com C++" → MSVC + Windows SDK)
- **LLVM/Clang** (para bindgen / asio-sys) — `C:\Program Files\LLVM\bin` no `PATH` e `LIBCLANG_PATH`

```powershell
$env:LIBCLANG_PATH = "$env:ProgramFiles\LLVM\bin"
$env:PATH = "$env:ProgramFiles\LLVM\bin;$env:PATH"
```

### Linux
- `libasound2-dev` (ALSA) ou JACK
- `clang` + `libclang-dev`

---

## Comandos rápidos

### Standalone (teste com microfone / interface)

```sh
# WASAPI (padrão Windows)
cargo run --bin prevocal-standalone

# ASIO (baixa latência, interface dedicada)
cargo run --features asio --bin prevocal-standalone
```

A barra **AUDIO** no topo permite escolher driver/dispositivo, reescanear (↻) e **RESTART** (aplica a troca). A última seleção persiste em `prevocal-last-device.txt`.

### Gerar plugins VST3 + CLAP (release)

```sh
cargo install cargo-nice-plug
cargo nice-plug bundle PreVocal --release
```

Bundles gerados em `target/bundled/`:
- `PreVocal.vst3`  (pasta VST3 bundle)
- `PreVocal.clap`  (arquivo CLAP)

### Instalar na pasta VST3 do sistema (Windows, requer PowerShell elevado)

```powershell
Start-Process powershell -Verb RunAs -Wait -ArgumentList '-NoProfile','-Command',"Copy-Item -LiteralPath 'D:\Dev\Porjetos\PreVocal\target\bundled\PreVocal.vst3\Contents\x86_64-win\PreVocal.vst3' -Destination 'C:\Program Files\Common Files\VST3\PreVocal.vst3' -Force; Copy-Item -LiteralPath 'D:\Dev\Porjetos\PreVocal\target\bundled\PreVocal.clap' -Destination 'C:\Program Files\Common Files\VST3\PreVocal.clap' -Force"
```

> **Feche a DAW antes** (o arquivo `.vst3` fica travado enquanto a DAW o carrega).

---

## Logs de diagnóstico

| Arquivo | Conteúdo |
|---------|----------|
| `C:\temp\prevocal_plugin.log` | Trace do DSP (append, apague antes de teste limpo) |
| `C:\temp\prevocal_load.log` | Log de carregamento do plugin (reescrito por instância) |
| `C:\temp\prevocal_panic.log` | Panic hook (acesso a código não mapeado, etc.) |

---

## Licença

Código proprietário — uso comercial mediante licença. Para distribuição gratuita (demo/avaliação) consulte o autor.

---

## Créditos

- **DSP & arquitetura**: autor do projeto
- **nice-plug**: framework VST3/CLAP em Rust (RustAudio)
- **Slint**: UI declarativa nativa
- **cpal**: áudio cross-platform (standalone)
- **Freeverb / Schroeder**: algoritmo de reverb (8 comb + 4 allpass, stereo spread escalado com SR)