Atue como um Engenheiro de Software Sênior especialista em Rust, DSP (Digital Signal Processing) e GUIs com Slint.

Preciso da estrutura de código completa, limpa e pronta para compilar do projeto **"PreVocal"** (Pré-amplificador de vocal em Rust).

### Requisito Principal de Framework:
Utilize a biblioteca **`nice-plug`** da organização RustAudio (hospedada no Codeberg: `https://codeberg.org/RustAudio/nice-plug`) como a abstração do plugin.

O projeto DEVE ser estruturado em **dois alvos (targets) no mesmo repositório**:
1. **Executável Standalone (`src/main.rs`):** Para rodar diretamente via `cargo run`, testando a GUI em Slint e o processamento de áudio em tempo real via sistema/microfone no Linux sem precisar de DAW.
2. **Biblioteca de Plugin (`src/lib.rs`):** Para expor a interface de plugin (VST3 e CLAP) para ser empacotada e usada em DAWs.

---

### 1. Especificações do Motor DSP (Rust):
Implemente o áudio em tempo real com suporte a suavização de parâmetros (smoothing):
- **Drive / Input Gain (0.0 a +24.0 dB):** Saturação macia (Soft Clipping) usando a função `tanh` com compensação de ganho para harmônicos quentes de válvula.
- **HPF / Low Cut (20 Hz a 200 Hz):** Filtro Passa-Altas Butterworth de 2ª ordem (12 dB/oitava) para limpeza dos sub-graves vocais.
- **Air / High Shelf (0.0 a +6.0 dB a 10 kHz):** Ganho de agudos para dar transparência e brilho ao vocal.
- **Phase Flip (0° / 180°):** Inversão de polaridade do sinal ($180^\circ$).
- **Output Trim (-12.0 a +12.0 dB):** Ganho de saída final.

---

### 2. Interface Gráfica no Slint (`ui/app.slint`):
- **Estilo Visual:** Dark Studio Pro / Anodized Metal com sotaques em Roxo Neon/Ultravioleta.
- **Paleta de Cores:**
  - Background: `#121214` (Grafite Escuro Fosco)
  - Cards/Painéis: `#1E1E24` (Cinza Escuro de Contraste)
  - Acento Principal: `#9D4EDD` (Roxo Neon) e `#C77DFF` (Highlight Purple)
  - Faixas/Detalhes: `#3C096C`
  - Textos: `#FFFFFF` (Título) e `#A0A0B0` (Rótulos)
- **Elementos do Layout:**
  - Header moderno com o título **PREVOCAL** e a indicação "Vocal Bus Preamp".
  - Sliders/Faders verticais estilizados com preenchimento roxo para **Drive** e **Output Trim**.
  - Knobs circulares estilizados para **HPF** e **Air**.
  - Botão com indicador luminoso roxo para **Phase Flip (180°)**.

---

### 3. Arquivos e Estrutura que Você Deve Gerar:

1. **`Cargo.toml`:** 
   - Dependência do `nice-plug` apontando para o repositório da RustAudio / Codeberg (`git = "https://codeberg.org/RustAudio/nice-plug"`).
   - Configurações do `slint` e `slint-build`.
   - Definição do target `[lib]` (cdylib/rlib) e do target `[[bin]]` (`prevocal-standalone`).
2. **`ui/app.slint`:** O arquivo completo com os componentes visuais, layout e tema roxo.
3. **`src/lib.rs`:** Estrutura base do plugin usando `nice-plug`, parâmetros, bindings da GUI e o algoritmo de DSP.
4. **`src/main.rs`:** Código de entrada para o modo Standalone rodar a janela do Slint e o áudio isoladamente via `cargo run`.
5. **Instruções de Execução:** Comandos exatos para testar o modo standalone e para gerar os bundles VST3/CLAP no Linux.

Escreva um código limpo, bem documentado e 100% aderente ao padrão da comunidade RustAudio.