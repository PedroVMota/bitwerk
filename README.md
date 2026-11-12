# bitwerk

Simulador de circuitos lógicos digitais para macOS: portas, interruptores, relógios, LEDs e fios numa grelha, com simulação orientada a eventos. É inspirado nas ferramentas educativas clássicas de lógica digital, mas o código e o formato de ficheiro são próprios.

> Estado: **esqueleto inicial**. Por agora há portas AND/OR/NAND/NOR/XOR/XNOR/NOT, interruptores, relógios e LEDs, desenhados com símbolos esquemáticos próprios (ANSI ou IEC), fios ponto-a-ponto, simulação com deteção de *race conditions*, gravação e abertura de projetos e um teste de carga com 100 mil componentes. Flip-flops, tri-state, memória, macros, junções de fios, desfazer/refazer e importação de `.dwm` ficam para depois.

## Prioridades

1. **Desempenho.** A interface nunca pode engasgar ao fazer zoom ou *pan*, nem ao manipular muitos objetos. A simulação nunca bloqueia a interface.
2. **Segurança do código.** Não há `unsafe`, todos os ficheiros são tratados como entrada hostil, não há rede e nada de um ficheiro é executado.

## Arquitetura

Workspace Cargo com cinco *crates*:

| Crate | Função |
|---|---|
| `crates/bw-core` | Modelo do circuito: ids, componentes, pinos, fios, posições numa grelha inteira. Só dados, serializáveis com serde. As invariantes (o fio vai de uma saída para uma entrada, cada entrada tem no máximo um condutor, os parâmetros estão dentro dos limites) são validadas na construção **e** na desserialização (`#[serde(try_from)]`). |
| `crates/bw-sim` | Motor de simulação. Compila o circuito numa *netlist* plana em *struct-of-arrays* (CSR para o *fan-out*) e avalia-a com atraso unitário e ciclos delta. Se a rede não estabiliza ao fim de `nós + 16` deltas, há *race condition* e a simulação para com erro. Corre numa thread própria. |
| `crates/bw-render` | Renderizador wgpu (Metal). Desenho instanciado com um número pequeno e **constante** de chamadas de desenho (grelha, fios e corpos: um quadrado por corpo ao longe, ou uma chamada indireta por tipo de componente com os símbolos). A câmara é um único *uniform* de 192 bytes. A cena tem *slots* estáveis e é atualizada no lugar a cada edição (instâncias, R-tree e GPU); o *culling* corre na GPU (*compute shader*). O módulo `symbols` define os símbolos esquemáticos. Ver [Símbolos](#símbolos) e [Renderização e desempenho](#renderização-e-desempenho). Os shaders WGSL estão em `src/shaders/`. |
| `crates/bw-io` | Formato de projeto (RON, com cabeçalho `format` e `version`) com limites rígidos e o módulo `dwm` (formato legado, ainda devolve "não suportado"). |
| `crates/bw-app` | Binário eframe/egui: paleta com ícones à esquerda, *canvas* GPU ao centro, propriedades à direita, barra com Run/Stop/Pause/Step, menu nativo do macOS (muda) e diálogos de ficheiro nativos (rfd). Inclui também o gerador do ícone da aplicação (`bw-icon`). |

### Modelo de threads

```
                    comandos (crossbeam, sem bloqueio)
  ┌──────────────┐  Load / Edit / SetSwitch / Run / Stop / Pause / Step / …
  │  UI (main)   │ ─────────────────────────────────────────────▶ ┌──────────┐
  │ winit+egui   │                                                 │  bw-sim  │
  │ + wgpu       │ ◀───────── Arc<Snapshot> via ArcSwap ────────── │ (thread) │
  └──────┬───────┘   (leitura wait-free; o sim nunca espera pela UI)└──────────┘
         │ Arc<Circuit> (só cargas completas)  ▲
         ▼                                     │ Box<Scene> (instâncias + R-tree)
  ┌──────────────┐ ────────────────────────────┘
  │  bw-scene    │  constrói a cena inteira ao abrir um ficheiro, criar um
  │  (thread)    │  circuito novo ou gerar o teste de carga, e liberta cenas
  └──────────────┘  antigas. As edições normais não passam por aqui.
```

- A UI **nunca** fica à espera de um lock. Envia comandos por canal e, em cada frame, lê o *snapshot* mais recente com `ArcSwap::load_full`.
- O motor publica *snapshots* no máximo a cada 8 ms e pede um *repaint* à UI. Corre os *ticks* em lotes limitados (10 000) para responder sempre a comandos. Se não consegue acompanhar o ritmo, descarta o atraso acumulado em vez de entrar em espiral.
- Uma edição (adicionar, mover, rodar, mudar o tipo, apagar, ligar, desligar) é um `bw_core::Edit`. A UI aplica-o à sua cópia do circuito, corrige a cena no lugar e envia o **mesmo** `Edit` ao simulador, que o aplica à cópia dele. Como os ids são atribuídos de forma determinística, as duas cópias ficam iguais sem nunca se partilhar nem clonar o circuito inteiro. Só abrir um ficheiro, criar um circuito novo ou gerar o teste de carga enviam o circuito completo (`Load`).
- O simulador agrupa as edições que encontra na fila e recompila a *netlist* uma única vez por lote. Um `Load` torna obsoletos os `Load`/`Edit` anteriores.
- Os níveis lógicos chegam à GPU como um bit por *slot* de corpo (32 por palavra) num *storage buffer*. Cada *snapshot* traz também o componente que conduz cada rede; a UI converte rede → *slot* uma vez por compilação. Enquanto o simulador recompila depois de uma edição, os componentes que não mudaram mantêm a cor (antes ficava tudo cinzento durante um ou dois frames).

### Símbolos

Cada componente tem o seu símbolo esquemático, desenhado de raiz para este projeto (não copia imagens nem *artwork* de outros produtos). Os símbolos são geometria vetorial definida em código (`crates/bw-render/src/symbols/`) e ficam **dentro do binário**: não há ficheiros externos nem *downloads*.

- **Dois estilos**: ANSI/IEEE 91 (formas distintivas, por omissão) e IEC 60617 (retângulos com `&`, `≥1`, `=1` e `1`, e bolha de inversão). Muda-se em **View → ANSI Symbols / IEC Symbols** ou nos botões ANSI/IEC da paleta (e `--style iec` no arranque). Mudar de estilo só altera um *uniform*: nenhuma malha nem instância é reenviada. O eframe não tem a *feature* de persistência ativa, por isso a escolha vale só para a sessão.
- **Componentes atuais**: AND, OR, NAND, NOR, XOR e XNOR com 2 a 8 entradas (no ANSI o lado das entradas cresce com uma linha de entrada prolongada, como nos esquemas reais; no IEC o retângulo cresce), NOT, bolhas de inversão, interruptor (alavanca aberta/fechada conforme o estado), relógio (onda quadrada, acesa quando está a 1) e LED (aceso, apagado ou cinzento quando flutuante/desligado).
- **Componentes futuros** (só o símbolo, sem simulação; aparecem na paleta em "Coming later", desativados): flip-flops D, RS e JK (caixa com triângulo de relógio e Q/Q̄), *buffer* tri-state (triângulo com pino de *enable*), RAM e ROM, mostrador de 7 segmentos, saída numérica, alimentação (VCC), massa, gerador de sequências, entrada interativa, macro/CI (caixa estilo DIL com N pinos e nome, `Symbol::macro_ic`), junção de fios e anotação de texto.
- **Pinos**: os pontos de ligação de cada símbolo coincidem exatamente com a geometria de pinos do `bw-core` (entradas em `(0, 2i + 1)`, saída em `(w, h/2)`, rotação igual a `Component::pin_pos`). Os fios terminam nos *stubs* dos pinos, que têm a mesma largura dos fios. Há testes que o verificam para todos os tipos, números de entradas e estilos.
- **Uma só fonte**: cada símbolo é descrito por caminhos (contorno + preenchimento) com um papel de cor por vértice. É tesselado **uma vez** (*ear clipping* para preenchimentos e faixas com *miter* para traços, feitos à mão, sem dependências como o `lyon`) para um **atlas estático partilhado** (um *array* de vértices com um sub-intervalo por tipo × número de entradas × estilo). A GPU usa esse atlas; a paleta pinta os mesmos vértices com o egui (malhas guardadas em *cache*, sem texturas); o ícone da aplicação é rasterizado a partir deles no CPU.
- **Traços nítidos em qualquer zoom**: cada vértice de traço tem um vetor de extrusão unitário que o *vertex shader* escala pela meia-largura do zoom atual (largura em unidades do mundo, mas nunca abaixo de cerca de 1 píxel físico). As etiquetas (`&`, `≥1`, Q, …) usam uma pequena fonte vetorial própria e ficam sempre direitas quando o componente roda.
- **Estado sem mudar malhas**: LED aceso, posição do interruptor, onda do relógio e cor do pino de saída vêm dos bits de nível por *slot* que já existiam. Cada vértice tem uma camada (sempre / só a 1 / só fora de 1) que o *shader* resolve.

### Renderização e desempenho

- **Slots estáveis.** Cada componente tem um *slot* de corpo e cada segmento de fio um *slot* de fio. Uma edição reescreve só os *slots* afetados (o componente e os fios ligados a ele). Os *slots* apagados ficam escondidos e vão para uma lista livre, que os novos objetos reutilizam primeiro. A R-tree (`rstar`, usada para *picking* e etiquetas) é atualizada com `remove`/`insert`. Quando mais de metade dos *slots* está livre, a cena é compactada com uma reconstrução em fundo.
- **Uploads incrementais.** A cena regista os *slots* sujos; o renderizador junta os vizinhos em blocos e só envia esses bytes. Mover um componente envia algumas centenas de bytes, não 11 MB.
- **Carregamento em *streaming*.** Uma cena nova (ficheiro, teste de carga) é enviada para a GPU em fatias de 1 MiB por frame (≈11 frames para 100k componentes), desenhando o que já chegou. O canto superior esquerdo mostra a percentagem. Os *bind groups* cobrem só a parte já enviada, para o wgpu não ter de preencher com zeros o resto dos *buffers* novos.
- **Culling na GPU.** Um *compute shader* em três passos (contar por *workgroup*, soma de prefixos, escrever) compacta os *slots* visíveis em listas de índices e preenche os argumentos dos `draw_indirect`. Os corpos visíveis ficam agrupados por tipo de componente (10 grupos, uns a seguir aos outros), o que dá ao mesmo tempo uma lista contínua para os quadrados e uma por tipo para os símbolos. Dentro de cada grupo a ordem é por *slot* e os grupos têm ordem fixa, por isso objetos sobrepostos nunca piscam. A lista é calculada para uma região com margem e reutilizada enquanto a vista lá couber e nada mudar, pelo que a maior parte dos frames de *pan* nem corre o *compute*. A CPU não faz trabalho por objeto ao navegar. Se a vista cobre a cena inteira, desenha-se tudo diretamente.
- **Símbolos e nível de detalhe.** A partir de 5 píxeis físicos por unidade de grelha (uma porta de 2 entradas tem então 20 px), cada tipo de componente é **uma** chamada `draw_indirect`. O `first_vertex` da chamada guarda o tipo nos bits altos do `vertex_index`, e cada instância procura no atlas a sua própria malha (tipo × entradas × estilo) e a rotação, que estão empacotadas no campo `kind` da instância (sem aumentar os 24 bytes). Ao todo são grelha + fios + 10 chamadas, seja qual for o número de objetos. Abaixo desse zoom mantém-se o LOD anterior (um quadrado por corpo, desenhado diretamente quando tudo está visível), por isso a vista afastada dos 100k não ficou mais lenta.
- **O *culling* é submetido à parte.** O egui-wgpu descarta o *encoder* do frame quando não obtém textura da superfície (ao abrir a janela ou com ela tapada). Por isso o passo de *compute* vai num *command buffer* próprio, submetido logo, e as listas só contam como calculadas depois disso. Antes podiam ficar vazias até à edição seguinte (via-se um *canvas* vazio em cenas pequenas).
- **Repaint a pedido.** Sem alterações, a app não desenha nada (0 % de CPU em repouso, medido com `ps`). Desenha-se quando há input, quando a câmara muda, quando o simulador publica um *snapshot*, quando chega uma cena ou enquanto se faz o *streaming*. Para medir, há `Debug → Continuous Repaint` ou `--continuous`. Com a janela tapada (*occluded*) ou minimizada, o eframe corre só a lógica e não apresenta nada, por isso nenhum vsync marca o ritmo: nesse estado os redesenhos pedidos ficam espaçados de pelo menos 250 ms, em vez de ocuparem um núcleo a 100 %.
- A cena antiga é libertada na thread `bw-scene` (libertar 460k instâncias e a R-tree custa milissegundos).

#### Medição reprodutível (`--stress --sweep`)

`bitwerk --stress --sweep [--sweep-csv frames.csv] [--window-pos X,Y]` carrega o teste de carga e, quando a cena está toda na GPU, corre um guião de cerca de 20 s: repouso com tudo visível, zoom até 40 pt/unidade, *pan*, zoom de volta, 200 edições (mover, rodar, adicionar + ligar, apagar, uma por frame) e repouso. No fim imprime uma tabela por fase (média, p50, p95, p99 e máximo do intervalo entre frames, frames > 16,7 ms e > 20 ms, vsyncs perdidos estimados, CPU por frame e tempo de *prepare*), mais a latência edição → ecrã, e fecha. A janela fica sempre por cima durante a medição. A taxa de *refresh* é detetada pela mediana dos intervalos (arredondada à taxa comum mais próxima). Se algum frame não foi apresentado (janela tapada ou minimizada) ou se a mediana não corresponde a nenhuma taxa plausível, o resumo diz **INVALID RUN** e as estatísticas só contam os frames apresentados. Os vsyncs perdidos estimados são o tempo total da fase em períodos de *refresh* menos os frames apresentados. Ao contrário dos intervalos isolados, não contam o *jitter* de ritmo (um frame atrasado seguido de um adiantado não custa nada).

Resultados num MacBook Pro M1 Pro, janela 1280×800 num monitor externo a 60 Hz (escala 1×), `--release`. Antes = código anterior (reconstrução completa por edição, *culling* na CPU, *upload* num frame). Valores típicos de duas execuções:

| fase / métrica | antes | depois |
|---|---|---|
| varrimento inteiro: CPU por frame p99 / máx | 21–22 / 40–63 ms | 2,8 / 3,8–8,7 ms |
| varrimento inteiro: vsyncs perdidos (20 s) | 4–5 | 1 |
| varrimento inteiro: intervalo p99 / máx | 22,4–22,8 / 40–63 ms | 20,6–21,2 / 22,6–25,6 ms |
| zoom (entrar/sair): CPU máx | 10–25 ms (consulta R-tree + *upload*) | 2,6–8,7 ms |
| 200 edições: intervalo p99 / máx | 28–29 / 40–63 ms | 17,1–18,0 / 17,6–20,1 ms |
| latência edição → ecrã | 225–232 ms (13 frames) | 1,3 ms (0 frames: no próprio frame) |
| carregamento: pior frame durante o *upload* | ≈60 ms (11 MB num frame) | ≤ 17 ms por frame (11 fatias) |
| repouso sem alterações | 1 núcleo a 100 % (repaint contínuo) | 0 % CPU, nenhum frame |

`make bench` (offscreen, MSAA 4×, cena de 100k, CPU *prepare* + GPU, mesma máquina):

| caso | antes | depois |
|---|---|---|
| tudo visível, 1280×800 | 2,9 ms | 3,6–3,9 ms |
| 2× zoom de ajuste | 2,7 ms | 1,7–1,9 ms |
| 4 pt/unidade | 0,4 ms | 0,6 ms |
| 30 pt/unidade | 0,4 ms | 0,5 ms |
| primeiro frame com a cena (*upload*) | 8,6–25,5 ms | ≤ 9,7 ms (pior das 11 fatias) |
| edição: modelo + cena | reconstrução em fundo de 113–150 ms | 0,01 ms (mover), 0,6 ms (apagar) |

Com os símbolos (mesma máquina e janela, `--stress --sweep --window-pos 300,200`, duas execuções; as duas válidas):

| métrica (varrimento inteiro) | antes dos símbolos | com símbolos |
|---|---|---|
| intervalo médio / p99 / máx | 16,68 / 20,52 / 31,57 ms | 16,68 / 20,10–21,19 / 23,20–23,83 ms |
| vsyncs perdidos (20 s) | 1 | 1 |
| CPU por frame média / p99 | 1,25 / 3,13 ms | 1,49–1,55 / 3,26–3,43 ms |
| latência edição → ecrã | 1,41 ms (0 frames) | 1,44–1,51 ms (0 frames) |

`make bench` com símbolos (a 1× os casos `fit`, `2x fit` e 4 pt/unidade usam quadrados e 30 pt/unidade usa símbolos; a 2× também 4 pt/unidade usa símbolos): `fit` 3,1 ms, `2x fit` 2,2–2,3 ms, 4 pt/unidade 0,8 ms (1×) e 1,1 ms (2×), 30 pt/unidade 0,8–0,9 ms, frame de edição 1,0–1,2 ms. São os mesmos valores de antes, dentro do ruído.

Outros números: a construção completa da cena (460k instâncias + R-tree) demora 135–150 ms em fundo. Compilação + estabilização inicial de 100k componentes no simulador: cerca de 18 ms (na thread dele). Um *tick*: microssegundos.

## Compilar, correr e testar

Requisitos: Rust estável (fixado em `rust-toolchain.toml`) e macOS com Metal.

```sh
make build        # cargo build --workspace
make run          # cargo run -p bw-app
make test         # cargo test --workspace
make lint         # cargo clippy --workspace --all-targets -- -D warnings
make release      # build otimizado (LTO thin, codegen-units=1, overflow-checks)
make bench        # medição headless do custo por frame com 100k objetos
make sweep        # medição automática de 20 s na app (abre uma janela)
make icon         # redesenha o ícone a partir do código (PNG + assets/AppIcon.icns via iconutil)
make bundle       # build release empacotado em target/bitwerk.app (Info.plist + ícone)
```

### Ícone e *bundle* macOS

O ícone (uma porta NAND com uma entrada a 1, outra a 0 e a saída acesa, sobre uma grelha) é desenhado por código a partir da mesma geometria dos símbolos (`symbols::raster::app_icon`, um rasterizador simples com *supersampling* 4×4). `make icon` corre o binário `bw-icon`, que escreve os PNG de todos os tamanhos do `.iconset` e `assets/AppIcon-256.png`, e depois o `iconutil` (que vem com o macOS) gera `assets/AppIcon.icns`. Os dois ficheiros estão no repositório, por isso um *build* normal não precisa deste passo. O PNG de 256 px vai embutido no executável (`include_bytes!`) e define o ícone da janela e do Dock através do `IconData` do eframe. `make bundle` cria `target/bitwerk.app` com `Contents/MacOS/bitwerk`, `Contents/Info.plist` (de `assets/Info.plist`) e `Contents/Resources/AppIcon.icns`, para o Finder e o Dock mostrarem o ícone. Nada é descarregado da rede.

O executável chama-se `bitwerk`. `bitwerk <ficheiro.bitwerk>` abre um projeto no arranque e `--style iec|ansi` escolhe o estilo dos símbolos. Opções de depuração:

- `--stress`: carrega o teste de 100k componentes no arranque e regista estatísticas de frame;
- `--continuous`: redesenha todos os frames (por omissão só se redesenha quando algo muda);
- `--sweep` (com `--stress`): medição automática, ver acima; `--sweep-csv <ficheiro>` grava os tempos de cada frame;
- `--window-pos X,Y`: posição da janela (para medir sempre no mesmo monitor);
- `--frame-latency N`: frames em fila para apresentação (experimental; o valor por omissão do eframe é 2);
- `--view X,Y,ZOOM`: posição e zoom da câmara quando a cena fica pronta (para capturas reprodutíveis).

### Utilização rápida

- **Paleta (esquerda):** escolher uma ferramenta ou componente (ícones com o símbolo; o nome aparece ao passar o rato) e clicar no *canvas* para o colocar. Enquanto se escolhe o sítio, o símbolo aparece em transparência. "Gate inputs" define o número de entradas das portas.
- **Select / move:** clicar para selecionar e arrastar para mover. Arrastar no vazio faz *pan*.
- **Interact:** clicar num interruptor muda o seu estado (também com a simulação parada).
- **Wire:** clicar num pino de saída e depois num pino de entrada. Uma saída pode ligar a várias entradas, mas nunca a outra saída. `Esc` cancela.
- **Zoom:** roda do rato, *pinch* no trackpad ou Cmd + scroll, sempre centrado no cursor. **Pan:** scroll de dois dedos, arrastar com o botão do meio ou o direito, ou arrastar no vazio.
- **Teclas:** `R` roda, `Del` ou `Backspace` apaga, F9 Run, F10 Stop, F11 Pause, F12 Step.
- **Menu Debug → Spawn Stress Test** (⇧⌘T): gera 100k componentes. O canto superior esquerdo mostra FPS, tempos de frame, o modo de desenho (tudo ou *culling* na GPU) e o progresso do *upload*. **Debug → Continuous Repaint** força o redesenho contínuo para medir.
- Há exemplos em `examples/half-adder.bitwerk` e em `examples/all-components.bitwerk` (todos os tipos atuais, vários números de entradas e rotações; `bitwerk examples/all-components.bitwerk --style iec` mostra o estilo IEC).

## Segurança

- `#![forbid(unsafe_code)]` em todos os *crates* (incluindo o alvo de fuzzing), mais `unsafe_code = "deny"` nos *lints* do workspace.
- Clippy `pedantic` em *warn* (e `-D warnings` no `make lint`). `unwrap_used` e `expect_used` dão aviso fora dos testes.
- `[profile.release]` com `overflow-checks = true`, `panic = "abort"`, `lto = "thin"` e `codegen-units = 1`.
- **Ficheiros = entrada hostil.**
  - O tamanho máximo (64 MiB) é verificado antes **e** durante a leitura.
  - A profundidade de recursão do RON é limitada (64).
  - Há limites de objetos (1M componentes, 2M fios, 256 vértices por fio, nome com 256 bytes) e de profundidade de macros (16, já no contrato do formato).
  - Há validação estrutural completa antes de qualquer dado chegar à aplicação.
  - Versões futuras dão um erro explícito.
  - A gravação é atómica (ficheiro temporário + `rename`).
- **Sem rede:** nenhuma dependência de rede. O eframe é compilado sem a *feature* `links`, por isso nem sequer abre URLs. **Nada de um ficheiro é executado.**
- Dependências de fontes conhecidas, verificadas com `cargo-deny`.

### Ferramentas de segurança

```sh
cargo install --locked cargo-deny     # uma vez
make audit                            # cargo deny check: advisories, licenças, bans, fontes

cargo install --locked cargo-fuzz     # uma vez (o fuzzing precisa de nightly)
make fuzz                             # cargo +nightly fuzz run load_project fuzz/seeds/load_project
make fuzz-check                       # só compila o crate de fuzzing (stable)
```

- `deny.toml` aceita só licenças permissivas (MIT, Apache-2.0, BSD, ISC, Zlib, …). A única exceção são as fontes embutidas do egui (OFL/Ubuntu Font License, que são dados). Recusa qualquer *advisory* RustSec e qualquer fonte fora do crates.io, e avisa de versões duplicadas.
- `fuzz/` é um workspace separado com o alvo `load_project`. Esse alvo verifica que o *loader* nunca entra em pânico e que tudo o que aceita volta a ser lido igual depois de gravado.

## Estrutura

```
bitwerk/
├── Cargo.toml            # workspace, lints, perfil release
├── rust-toolchain.toml   # stable
├── deny.toml             # cargo-deny
├── Makefile
├── crates/
│   ├── bw-core/          # modelo
│   ├── bw-sim/           # motor de simulação (+ tests/sim.rs)
│   ├── bw-render/        # renderizador wgpu (+ shaders WGSL, símbolos, testes GPU)
│   ├── bw-io/            # formato de ficheiro (+ dwm stub)
│   └── bw-app/           # aplicação eframe
├── assets/               # ícone gerado por código (AppIcon.icns, AppIcon-256.png) e Info.plist
├── examples/             # half-adder.bitwerk, all-components.bitwerk
└── fuzz/                 # cargo-fuzz
```

## Limitações conhecidas (deste esqueleto)

- Apagar um componente percorre todos os fios do modelo (`Circuit::remove_component` é O(fios): cerca de 0,6 ms com 180k fios). A cena só toca nos fios ligados.
- O simulador recompila a *netlist* inteira a cada lote de edições (≈20 ms para 100k componentes, na thread dele). As cores das partes editadas chegam com esse atraso; a geometria aparece logo.
- Com tudo visível, a GPU desenha cada instância (≈3,5 ms com 460k num ecrã 1×). Ainda não há um LOD agregado para zoom muito afastado.
- O intervalo entre frames tem *jitter* de cerca de ±3 ms (intervalos de 19–22 ms seguidos de outros curtos), também quando a CPU e a GPU quase não trabalham. Vem do ritmo de apresentação do macOS (`nextDrawable`), não da app, e quase nunca resulta num vsync perdido.
- Ao arrancar: o primeiro frame gasta ≈28 ms de CPU (atlas de fontes do egui) e o seguinte espera ≈100 ms pela primeira apresentação da janela. Há também um frame de ≈35 ms, com 0,5 ms de CPU na UI, no instante em que a construção da cena de 100k termina em fundo.
- Os fios são ponto-a-ponto, com vértices só visuais. Ainda não há junções nem edição de vértices.
- Entradas desligadas valem 0. Ainda não há estado "flutuante" nem X.
- Ainda não há seleção múltipla, copiar/colar, desfazer/refazer nem anotações.
- Com a simulação parada, o circuito combinacional continua a ser avaliado depois de cada edição, o que dá feedback imediato. O tempo (relógios) só avança com Run ou Step.
- O menu nativo só existe em macOS. Noutras plataformas, a barra egui mostra Open/Save.
- Símbolos: os *stubs* dos pinos de entrada têm a cor do contorno do componente, e não a do nível do fio, porque a instância só guarda um nível (o da saída). O fio que chega ao pino tem a cor certa. O atlas tem um macro/CI genérico ("IC", 4+4 pinos). Macros com nome próprio precisam de `Symbol::macro_ic` e ainda não estão no atlas da GPU. Os símbolos futuros não têm tipo no `bw-core` nem simulação. A escolha ANSI/IEC não é gravada entre sessões.
