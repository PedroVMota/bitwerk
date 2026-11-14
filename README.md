# bitwerk

Simulador de circuitos lógicos digitais para macOS: portas, interruptores, relógios, LEDs, *buffers* tri-state, flip-flops, RAM/ROM, **macros** (sub-circuitos hierárquicos) e fios ortogonais com junções numa grelha, com simulação orientada a eventos e lógica de quatro estados (0, 1, Z, X). É inspirado nas ferramentas educativas clássicas de lógica digital, mas o código e o formato de ficheiro são próprios.

> Estado: **em desenvolvimento**. Há portas AND/OR/NAND/NOR/XOR/XNOR/NOT, interruptores, relógios, LEDs, *buffers* tri-state com barramentos, flip-flops D/RS/JK (latch, master-slave e por flanco, com *preset*/*clear* assíncronos), RAM e ROM com janela de conteúdo e ficheiros `.map`, desenhados com símbolos esquemáticos próprios (ANSI ou IEC), fios ortogonais com ramificações e junções (pontos de solda), caminhos de fios editáveis, simulação com deteção de *race conditions* e de *bus contention*, gravação e abertura de projetos, **macros hierárquicas** (definições referenciadas e não copiadas, navegação para dentro de uma instância com o estado ao vivo, criação a partir de uma seleção, modelos em caixa ou DIL, *datasheets*) com o **Parts Centre** e uma biblioteca de 20 peças incluída, alimentação (VCC) e massa, geradores de sequências, botões de pressão e entradas de impulsos, mostradores de 7 segmentos e saídas numéricas, anotações de texto, cores de LEDs e mostradores, um modo de simulação compatível com o Digital Works, a **importação de circuitos e macros do Digital Works** (`.dwm`, `.dwt` e `.map`, ver [Importação do Digital Works](#importação-do-digital-works)) e a **exportação** para `.dwm` e `.map` binário (ver [Exportação para o Digital Works](#exportação-para-o-digital-works)) e testes de carga com 100 mil componentes e com 37 500 instâncias de macros. Desfazer/refazer fica para depois.

## Prioridades

1. **Desempenho.** A interface nunca pode engasgar ao fazer zoom ou *pan*, nem ao manipular muitos objetos. A simulação nunca bloqueia a interface.
2. **Segurança do código.** Não há `unsafe`, todos os ficheiros são tratados como entrada hostil, não há rede e nada de um ficheiro é executado.

## Arquitetura

Workspace Cargo com cinco *crates*:

| Crate | Função |
|---|---|
| `crates/bw-core` | Modelo do circuito: ids, componentes, pinos, macros (definições, *tags*, instâncias e a tabela de definições, ver [Macros](#macros-sub-circuitos-hierárquicos)), o grafo de fios (nós e segmentos ortogonais, ver [Fios](#fios-segmentos-nós-e-junções)), posições numa grelha inteira, conteúdo das memórias. Só dados, serializáveis com serde. As invariantes (segmentos horizontais ou verticais, nós soltos com pelo menos dois segmentos, no máximo um condutor por rede — exceto saídas tri-state, que podem partilhar a rede —, os parâmetros estão dentro dos limites, o conteúdo de cada memória tem o tamanho e a largura dela) são validadas na construção, em cada edição **e** na desserialização (`#[serde(try_from)]`). |
| `crates/bw-sim` | Motor de simulação. Achata a hierarquia de macros (`flatten.rs`, com *templates* em *cache* por definição) e compila o circuito numa *netlist* plana em *struct-of-arrays* (CSR para o *fan-out* e para os condutores de cada rede) e avalia-a com atraso unitário, ciclos delta e lógica de quatro estados (`logic.rs`). Se a rede não estabiliza ao fim de `nós + 16` deltas, há *race condition*; se dois condutores tri-state ativos ficam na mesma rede depois de estabilizar, há *bus contention*. Nos dois casos a simulação para com erro. Corre numa thread própria. |
| `crates/bw-render` | Renderizador wgpu (Metal). Desenho instanciado com um número pequeno e **constante** de chamadas de desenho (grelha, fios e corpos: um quadrado por corpo ao longe, ou uma chamada indireta por tipo de componente com os símbolos). A câmara é um único *uniform* de 192 bytes. A cena tem *slots* estáveis e é atualizada no lugar a cada edição (instâncias, R-tree e GPU); o *culling* corre na GPU (*compute shader*). O módulo `symbols` define os símbolos esquemáticos. Ver [Símbolos](#símbolos) e [Renderização e desempenho](#renderização-e-desempenho). Os shaders WGSL estão em `src/shaders/`. |
| `crates/bw-io` | Formato de projeto (RON, com cabeçalho `format` e `version`, versão 5 com migração da 4, 3, 2 e 1), o formato `.bwmacro` (uma macro com as dependências), a biblioteca de peças (`library`: incluídas no binário e as do utilizador), a validação dos *links* de *datasheet* (`datasheet`) com limites rígidos, o formato `.map` de conteúdo de memória (`memmap`, que lê também os `.map` binários do Digital Works) e a importação e exportação do Digital Works (`dwm`: `reader` lê o *stream* de objetos, `convert` constrói o modelo, `export` constrói o grafo de objetos a partir do modelo e `writer` escreve-o com os mesmos programas de campos do leitor; ver [Importação do Digital Works](#importação-do-digital-works) e [Exportação para o Digital Works](#exportação-para-o-digital-works)). |
| `crates/bw-app` | Binário eframe/egui: navegação na hierarquia (`hier.rs`), Parts Centre (`parts.rs`), propriedades de macros (`macro_props.rs`), paleta com ícones à esquerda, *canvas* GPU ao centro, propriedades à direita, barra com Run/Stop/Pause/Step, menu nativo do macOS (muda) e diálogos de ficheiro nativos (rfd). Inclui também o gerador do ícone da aplicação (`bw-icon`). |

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
- O conteúdo das memórias **nunca** vai nos *snapshots*. Há um segundo `ArcSwap` só para a memória cuja janela está aberta (`Command::WatchMemory`): o motor copia-a no máximo a cada 100 ms e só quando mudou (cada memória tem um contador de alterações). Com a janela fechada, o custo é zero.
- Uma edição (adicionar, mover, rodar, mudar o tipo, apagar, desenhar um fio, apagar um segmento, mover uma junção ou uma curva, arrastar um segmento, escrever na memória) é um `bw_core::Edit`. A UI aplica-o à sua cópia do circuito, corrige a cena no lugar com o que o modelo diz ter mudado (`Applied`: componentes, segmentos, nós e pinos tocados) e envia o **mesmo** `Edit` ao simulador, que o aplica à cópia dele. Como os ids são atribuídos de forma determinística (também os dos nós e segmentos, reutilizados por listas livres LIFO), as duas cópias ficam iguais sem nunca se partilhar nem clonar o circuito inteiro. Só abrir um ficheiro, criar um circuito novo ou gerar o teste de carga enviam o circuito completo (`Load`).
- O simulador agrupa as edições que encontra na fila e recompila a *netlist* uma única vez por lote. Um `Load` torna obsoletos os `Load`/`Edit` anteriores. As edições só geométricas não recompilam: mover e rodar componentes, mover junções e curvas, arrastar segmentos e editar a memória (nenhuma muda que pinos estão ligados; há um teste que o verifica com a *thread* do simulador). Ao recompilar depois de uma edição, o estado dos flip-flops e o conteúdo das memórias passam para a nova compilação (por id de componente), por isso editar um circuito a correr não apaga o contador nem a RAM. Só `Stop` (ou abrir outro circuito) repõe tudo.
- Os níveis lógicos chegam à GPU com **2 bits por *slot* de sinal** (16 por palavra) num *storage buffer*. Cada pino de saída tem um *slot* de sinal (os de um componente são contíguos, por isso Q e Q̄ ou os 16 bits de dados de uma memória são `net`, `net + 1`, …). Cada *snapshot* traz os níveis já empacotados a 2 bits e, partilhado por compilação, a lista de pinos que conduzem cada rede (vários num barramento). A UI converte rede → *slots* uma vez por compilação. Enquanto o simulador recompila depois de uma edição, os componentes que não mudaram mantêm a cor.

### Modelo de sinais (quatro estados)

Cada rede tem um de quatro valores, guardado em 2 bits:

| valor | significado | cor do fio | LED |
|---|---|---|---|
| 0 | nível baixo | verde escuro | apagado |
| 1 | nível alto | verde claro | aceso (cor do LED) |
| Z | flutuante / alta impedância: ninguém conduz a rede (tri-state inativo) ou a entrada não está ligada | cinzento-azulado | cinzento ("terceira cor" do original) |
| X | desconhecido ou em conflito (por exemplo RS = 11, dois condutores, entrada desconhecida) | laranja | laranja |

- **Entradas:** uma entrada flutuante (Z) lê-se como X. Uma entrada sem fio está flutuante. (No formato 1, uma entrada desligada valia 0; ver [Formato do projeto](#formato-do-projeto).)
- **Portas:** regras habituais de dominância. AND com algum 0 dá 0, OR com algum 1 dá 1, NAND/NOR igual mas negado. Fora disso, qualquer entrada X dá X. XOR/XNOR/NOT dão X com qualquer entrada desconhecida.
- **Pinos de controlo opcionais:** quando *não estão ligados*, o *preset*/*clear* dos flip-flops ficam inativos (0), o C̄S̄ da memória fica selecionado (0) e o R/W̄ fica em leitura (1). Ligados a uma rede flutuante, leem-se como X. Assim um flip-flop ou uma ROM funcionam sem fios de controlo, e um controlo ligado mas indefinido continua a ser visível.
- **Elementos sequenciais e X:** os flip-flops e a memória avaliam X de forma exata. Uma entrada desconhecida só dá X se os valores possíveis discordarem (por exemplo, JK com J = 1 e K = 0 dá 1 mesmo com Q desconhecido). Um flanco "talvez" (0 → X ou X → 1) só torna Q desconhecido se o novo valor for diferente do atual.
- **Arranque:** depois de um *reset*, todas as redes começam flutuantes e o circuito é avaliado de uma vez. Nesse instante não há flancos de relógio e um relógio/*preset*/*clear* ainda indefinido não estraga o estado guardado. Os flip-flops arrancam com Q = 0 e as memórias com o seu conteúdo de projeto.
- A deteção de *race conditions* é a mesma de antes (mais de `nós + 16` deltas).

#### Modos de avaliação (`EvalMode`)

Cada projeto escolhe como as alterações se propagam (menu *Circuit*, gravado no ficheiro como `eval: Sequential`):

- **Atraso unitário** (`UnitDelay`, por omissão): o modo descrito acima. Cada porta demora um ciclo delta e todas as mudanças de um delta são aplicadas ao mesmo tempo (Jacobi). É o modo nativo, com os quatro estados completos: uma entrada flutuante lê-se como X e tudo arranca flutuante.
- **Sequencial** (`Sequential`, "compatível com o Digital Works"; os projetos importados de `.dwm` abrem neste modo): os nós pendentes são avaliados um a um e cada saída nova fica **logo** visível para os nós avaliados a seguir no mesmo varrimento (Gauss–Seidel). Duas portas acopladas em cruz cujas entradas mudam ao mesmo tempo resolvem-se assim por uma ordem determinística (a ordem das redes no *fan-out*), em vez de oscilarem em passo certo. Como no Digital Works, o circuito arranca com **todas as redes a 0** e uma entrada flutuante ou sem fio **lê-se como 0**. Os flip-flops, memórias, barramentos e o resto da semântica de quatro estados (X de conflitos, Z dos tri-state) são os mesmos.
- **Porquê:** os circuitos de exemplo do Digital Works constroem *latches* com portas (por exemplo um JK master-slave com oito NAND). Com atraso unitário, o *latch* RS de duas NAND cujas entradas voltam a 1 ao mesmo tempo oscila para sempre (*race condition*), e a partir do arranque flutuante fica em X. No modo sequencial assenta, como no programa original. Os testes `bw-sim/tests/dwm.rs` e `components.rs` verificam os dois comportamentos.
- **Deteção de *race conditions*:** a mesma regra nos dois modos (mais de `nós + 16` deltas, ou varrimentos no modo sequencial). Uma oscilação verdadeira (uma NAND realimentada pela própria saída) continua a parar a simulação no modo sequencial; um *latch* que assenta já não. O *bus contention* é verificado depois de estabilizar, nos dois modos.
- **Custo:** o modo de atraso unitário mantém exatamente o mesmo caminho quente (o modo é uma constante de compilação da função de avaliação), por isso o teste de carga não muda (`stress_sim_cost`: *tick* 12,2–12,7 µs antes e depois).

### Fios: segmentos, nós e junções

A ligação de cada rede é um **grafo de segmentos ortogonais** (`bw_core::wiring`):

- **Nós** (`NodeId`): um pino de componente, uma **junção** (ponto de solda) ou um **vértice de curva**. Um pino tem no máximo um nó, que fica sempre em cima do pino.
- **Segmentos** (`SegmentId`): uma linha horizontal ou vertical entre dois nós. Só as **pontas** ligam: dois fios que se cruzam ou se sobrepõem sem partilhar um nó não estão ligados (como num esquema).
- **Junção:** onde se encontram três ou mais segmentos (um nó solto com três ou mais, ou um pino com dois ou mais, porque o *stub* do pino conta). O ponto é desenhado automaticamente.
- **Rede elétrica** = componente ligada do grafo. Regras (as de antes): no máximo um condutor que não seja tri-state, ou só condutores tri-state; nunca duas saídas normais juntas.
- **Normalização** depois de cada edição, só à volta do que mudou: pontas soltas são apagadas (até ao pino ou à junção seguinte), uma curva cujos dois segmentos ficaram alinhados desaparece (os dois segmentos fundem-se), segmentos de comprimento zero juntam os dois nós (dois pinos nunca se fundem) e segmentos duplicados entre os mesmos dois nós são removidos.
- **Edições** (todas por `Edit` / `Circuit::apply`, que valida tudo antes de mudar alguma coisa: se falhar, o circuito fica igual):
  - `AddWire { from, to, via }`: as pontas são um pino, um nó existente ou um ponto **no meio de um segmento** (`WireEnd::OnSegment`), que é dividido em dois e ganha uma junção. Os pontos `via` são as curvas; se dois pontos seguidos não estiverem alinhados, entra um canto (horizontal primeiro). Juntar duas redes é validado: com dois condutores normais é recusado com "cannot join two outputs on one net (component #a pin x and component #b pin y): only tri-state outputs may share a net". Tri-states juntam-se num barramento.
  - `RemoveSegment`: apaga um segmento; o que fica pendurado é limpo e as curvas alinhadas fundem-se. Pode partir uma rede em duas.
  - `MoveNode`: move uma junção ou curva (os pinos não se movem sozinhos).
  - `DragSegment(s, d)`: desloca um segmento na perpendicular. As pontas que podem acompanhar (curvas cujos outros segmentos são perpendiculares) deslizam; as outras (pinos, junções) ficam e ganham um segmento de ligação.
  - Mover, rodar ou mudar o tipo de um componente faz ***rubber-banding***: cada segmento ligado a um pino que deixaria de ser ortogonal faz deslizar a curva da outra ponta (se o outro segmento dela for perpendicular) ou ganha um canto novo (um L). Os fios continuam ligados aos mesmos pinos.
- **Custo:** cada edição trabalha sobre a adjacência dos nós tocados; juntar redes percorre só as redes envolvidas. Nada é O(total de fios): apagar um componente passou de ≈0,6 ms (percorria todos os fios) para ≈0,003 ms com 100k componentes. Os nós guardam até quatro segmentos sem alocar memória.
- A cena da GPU segue o mesmo modelo: um *slot* por segmento e um *slot* de corpo por junção. A cor de cada segmento (e de cada ponto e de cada LED) é o *slot* de sinal do condutor da rede (o menor pino de saída, para o resultado ser determinístico) ou "sem rede" (flutuante, Z) se a rede não tiver condutor. Depois de uma edição de ligações, as redes tocadas são repintadas (custo: o tamanho dessas redes); depois de uma edição só geométrica, os segmentos novos herdam a cor de um vizinho.

### Tri-state e barramentos

- **Tri-state** (doc 4.2): entradas IN e EN (EN no topo do símbolo) e uma saída. Com EN = 1 a saída segue IN; com EN = 0 fica **flutuante** (Z); com EN desconhecido, X. IN desconhecido com EN = 1 dá X.
- **Barramento:** só saídas tri-state (os *buffers* tri-state e as saídas de dados das memórias) podem partilhar uma rede. O `bw-core` recusa juntar uma saída normal a uma rede que já tem condutor, e vice-versa. Um barramento desenha-se como uma linha com junções: cada saída tri-state e cada entrada liga-se a ela. O simulador junta os condutores de cada rede do grafo de fios e resolve-a sempre que um condutor muda: nenhum ativo → Z; exatamente um → o valor dele; mais do que um → X.
- **Bus contention** (doc 6.6): se, **depois de o circuito estabilizar**, dois ou mais condutores da mesma rede estão a conduzir 0/1, a simulação **para** com "Bus contention, process halted", o número da rede e os componentes em conflito (por tipo e id). Esses componentes ficam contornados a vermelho no *canvas*. A verificação é feita no fim de cada estabilização, e não a cada delta. Assim, dois *enables* complementares que mudam por portas com atrasos diferentes (sobrepõem-se durante um delta) não dão um falso alarme. Um condutor a X faz a rede X, mas não conta como conflito.
- O exemplo `examples/bus-contention.bitwerk` tem os dois *enables* ligados ao mesmo interruptor (ativo): ao abrir, a simulação para logo com o erro.

### Flip-flops

Três tipos (doc 4.3), cada um com quatro modos de disparo (propriedade *trigger*), saídas Q e Q̄ (Q̄ é sempre o complemento de Q; com Q = X, as duas são X) e, opcionalmente, *preset* (topo) e *clear* (base) **assíncronos e ativos a 1**.

| tipo | entradas | tabela (quando o flip-flop "toma" as entradas) | disparo por omissão (doc) |
|---|---|---|---|
| D | D, C | Q ← D | flanco ascendente ("master-slave, saída no flanco ascendente": funcionalmente um D por flanco ascendente) |
| RS | S, C, R | 00 mantém, S = 1 SET, R = 1 RESET, **11 proibido → Q = Q̄ = X** | *latch* (o biestável da doc) |
| JK | J, C, K | 00 mantém, 01 RESET, 10 SET, 11 *toggle* | master-slave, saída no flanco descendente |

Modos de disparo:

- **Latch (biestável, sensível ao nível):** transparente enquanto C = 1, memoriza com C = 0. Não existe para o JK (com J = K = 1 oscilaria).
- **Master-slave (por impulso):** o *master* segue as entradas enquanto C = 1 e o *slave* (Q) copia o *master* no flanco descendente. No JK, o *master* recebe a realimentação do *slave* (só faz SET com Q = 0 e só faz RESET com Q = 1), por isso muda no máximo uma vez por impulso. Isto é o *ones catching*: um impulso em J com C = 1 fica memorizado, ao contrário do que acontece num JK por flanco.
- **Flanco ascendente / descendente:** as entradas são lidas só no flanco de C.
- **Preset/clear:** atuam sem relógio e dominam-no enquanto estão ativos. Os dois ativos ao mesmo tempo dão Q = Q̄ = X (a doc não diz nada; é o caso indeterminado habitual). Sem fio estão inativos.
- O símbolo mostra o modo: `C` simples (latch), triângulo dinâmico (flanco), triângulo com bolha (flanco descendente), ou `C` simples com a marca de saída adiada ⌐ junto a Q e Q̄ (master-slave). PRE e CLR aparecem no topo e na base.
- **Step** (doc 6.4): ½ ciclo avança até ao próximo flanco de relógio e um ciclo completo até ao segundo. Um flip-flop por flanco ascendente muda num passo de meio ciclo sim e no seguinte não. Há um teste que o verifica com a *thread* do simulador.

### Memória (RAM e ROM)

- **Tamanho** (doc 4.4): 2ⁿ palavras com **n = 4 … 14** linhas de endereço (16 a 16 384 palavras) e **1, 2, 4, 8 ou 16 bits** por palavra. A doc não dá limites para os dados; o diálogo original tem um seletor com 5 posições (0–4), que corresponde a estas larguras. Por omissão: 8 × 8 (256 palavras de 8 bits, como no original).
- **Pinos** (de cima para baixo à esquerda): A0…A(n−1), depois, só na RAM, D0…D(d−1) (dados de entrada) e R/W̄; por fim C̄S̄. À direita ficam as saídas Q0…Q(d−1), que são **tri-state** (podem formar barramento).
- **Semântica** (doc: R/W̄ baixo = escrita, C̄S̄ ativo a 0):
  - C̄S̄ = 1: saídas flutuantes (Z) e nada é escrito.
  - C̄S̄ = 0 e R/W̄ = 1: leitura. As saídas mostram a palavra no endereço (X se algum bit de endereço é desconhecido).
  - C̄S̄ = 0 e R/W̄ = 0 (só RAM): escrita por nível, como numa SRAM assíncrona. Enquanto dura, a palavra segue os dados de entrada (bits X/Z são guardados como 0). As saídas ficam flutuantes durante a escrita. Não há pino OE separado: a saída só é ativada numa leitura selecionada, ou seja, R/W̄ faz também de OE.
  - C̄S̄ ou R/W̄ desconhecidos: saídas X e não se escreve nada. Endereço desconhecido numa escrita: não se escreve nada.
  - A ROM não tem D nem R/W̄. Está sempre em leitura quando selecionada.
- **Armazenamento:** no simulador, um `Vec<u16>` por instância (no máximo 16 384 palavras, 32 KiB), validado contra os limites. O conteúdo de **projeto** vive no modelo (`Circuit`, partilhado *copy-on-write*) e é gravado no ficheiro. As escritas da RAM durante a simulação mudam só o conteúdo "vivo": `Stop` repõe o conteúdo de projeto e gravar o projeto não guarda o que a RAM escreveu. Editar uma célula na janela muda os dois.
- **Janela Memory Contents** (Properties → *Edit contents…*, ou `--memory-window ID`):
  - Grelha virtualizada (só as linhas visíveis são desenhadas), por isso 16K palavras não custam mais do que 16. Mostra o endereço (`$0000`) e a palavra em **hexadecimal** ou **binário**.
  - Clicar numa célula edita-a. Enter grava e passa à seguinte, Esc ou clicar fora cancela. Um valor inválido ou mais largo do que a palavra é recusado com uma mensagem.
  - Atualiza-se em direto enquanto a simulação corre (feed próprio, no máximo a cada 100 ms; "live"/"design" indica a origem).
  - **Clear…** pede confirmação ("Clear all memory contents?"). **Load…** e **Save…** usam ficheiros `.map`. Um mapa maior do que a memória é **truncado** (como na doc), um menor é completado com zeros e as palavras mais largas são cortadas à largura da memória. A janela diz o que aconteceu.

#### Formato `.map`

Texto simples, definido para este projeto:

```text
# bitwerk memory map          (linhas começadas por # ou ; são comentários)
bitwerk-map 1                 (identificador e versão; primeira linha útil)
width 8                       (bits por palavra, 1..=16)
words 256                     (número de palavras, 1..=16384)
0000: 3e 00 c3 10 ...         (endereço: palavras, tudo em hexadecimal)
0010: ...
```

As linhas de dados começam pelo endereço da primeira palavra e dois pontos, seguidos de palavras em hexadecimal separadas por espaços. Os endereços têm de crescer (um salto deixa zeros) e as palavras que faltam valem 0. O gravador escreve 16 palavras por linha. O *parser* trata o ficheiro como hostil: no máximo 1 MiB e 4 KiB por linha, *tokens* com até 4 dígitos, valores dentro da largura, endereços dentro do tamanho, nada de dados antes do cabeçalho nem cabeçalhos repetidos. Tem testes de ficheiros malformados e um alvo de *fuzzing* (`load_memory_map`).

### Formato do projeto

- **Versão 5** (atual): acrescenta os componentes novos (`Const(high: true)` = VCC, `Const(high: false)` = massa, `SeqGen(pattern: 11, len: 4, half_period: 10)`, `SevenSeg(common_anode: false)`, `Numeric(bits: 8, format: Hex, signed: false)`, `Text(w: 9, h: 3)`), o modo dos interruptores (`Switch(on: false, mode: Momentary)` ou `Pulse`; omitido para `Toggle`), a cor de um componente (`color: Some(Blue)`, omitida por omissão), as anotações (`notes: [(id: (4), note: (text: "Olá", height: 12, bold: true))]`) e o modo de simulação (`eval: Sequential`, omitido para o atraso unitário). Tudo tem valor por omissão, por isso os ficheiros 4 abrem sem conversão; um ficheiro "versão 4" que use alguma destas coisas é recusado. Antes desta mudança foram gravadas *fixtures* do formato 4 com o código anterior (`crates/bw-io/tests/fixtures/v4-*.bitwerk`, os cinco exemplos, incluindo `macros-demo` com macros): um teste verifica que abrem com os mesmos componentes, redes e *hashes* de macros, gravam como 5 e reabrem iguais. Os *hashes* de conteúdo das macros não mudam (a cor e a nota só entram no *hash* quando existem). O `.bwmacro` passa à versão 2 (os registos do formato 5); a versão 1 continua a abrir.
- **Versão 4:** acrescenta macros. O ficheiro tem uma tabela `macros` com as definições usadas, por isso o projeto é autossuficiente e portável. Cada definição tem `id`, `name`, `pins` (`(tag: (0), name: "A", dil: 0)`), `package` (`Dil(14)`; omitido para a caixa), `datasheet`, `doc_link` e `origin` (opcionais), o `hash` de conteúdo (informativo; é recalculado ao carregar) e `body`, que é um circuito como o da raiz. As instâncias escrevem-se `kind: Macro((3))` e as *tags* `kind: Tag(dir: In, tristate: false)`. Ao carregar, as definições são construídas pela ordem das dependências (cada uma depois das que usa) e verificam-se ciclos, referências desconhecidas, profundidade, tamanho achatado (por definição e da raiz), número de definições, pinos e tamanho do *datasheet* antes de o projeto chegar à app.
  - **`.bwmacro`:** uma macro com todas as dependências embutidas (`format: "bitwerk-macro", version: 1, root: (id), macros: [...]`, com os mesmos registos). Um ficheiro com definições que a raiz não usa é recusado. Ao importar, as definições com o mesmo *hash* já presentes no projeto são reutilizadas e as outras recebem ids livres.
  - **Migração da versão 3:** não há conversão (os ficheiros 3 não têm macros); um ficheiro "versão 3" que use *tags* ou macros é recusado. Antes desta mudança foram gravadas *fixtures* do formato 3 (`crates/bw-io/tests/fixtures/v3-*.bitwerk`, os quatro exemplos); um teste verifica que abrem com os mesmos componentes e redes, gravam como 4 e reabrem iguais. Todos os formatos antigos são gravados como 4.
- **Versão 3:** os fios passam a ser um grafo. Em vez de `wires`, o circuito tem `nodes` e `segments`:

  ```ron
  nodes: [
      (id: (0), at: Pin((component: (0), pin: 0))),   // nó num pino (a posição é a do pino)
      (id: (1), at: Free((x: 4, y: 1))),              // junção ou curva
      (id: (2), at: Pin((component: (2), pin: 0))),
  ],
  segments: [(a: (0), b: (1)), (a: (1), b: (2))],
  ```

  Ao carregar valida-se tudo: ids únicos, cada pino com no máximo um nó, segmentos entre dois nós diferentes, horizontais ou verticais (comprimento zero só entre dois pinos), sem duplicados, nós soltos com pelo menos dois segmentos (nada de pontas penduradas) e a regra dos condutores em cada rede. Curvas alinhadas escritas à mão são normalizadas. A gravação renumera os nós de forma densa, por isso gravar → abrir → gravar dá o mesmo texto. Limites: 4 milhões de segmentos, 4 milhões de nós e 1 milhão de junções por ficheiro (além dos 64 MiB).
- **Migração da versão 2 (e 1):** cada fio ponto-a-ponto antigo passa a ser um caminho ortogonal (reto ou em L) entre os seus dois pinos; os vértices antigos são mantidos e, se dois seguidos não estiverem alinhados, ganham um canto (horizontal primeiro). Fios que partilhavam um pino partilham o nó desse pino, por isso o *fan-out* e os barramentos tri-state continuam a ser as mesmas redes (o pino fica com um ponto de junção). Um fio antigo tem de ir de uma saída para uma entrada, como antes. Antes desta mudança foram gravadas *fixtures* no formato 2 (`crates/bw-io/tests/fixtures/v2-*.bitwerk`, os quatro exemplos); há testes que verificam que todas as *fixtures* (1 e 2) abrem, que as redes ficam exatamente as mesmas (comparadas com um *union-find* sobre os fios antigos), que cada nó de pino está em cima do pino e que gravam e reabrem iguais no formato 3. Os ficheiros 1 e 2 são gravados como 3.
- **Versão 2:** acrescenta os tipos `TriState`, `FlipFlop(ty, trigger, preset_clear)` e `Memory(ty, addr_bits, data_bits)`, e a lista `memories` com o conteúdo de cada memória não vazia. O conteúdo vai codificado de forma compacta (`bw_core::memenc`): palavras em hexadecimal, sequências iguais como `valor*N` e zeros finais omitidos (por exemplo `"12 0*3 ab*3"`). Uma ROM de 16K palavras quase vazia ocupa poucas dezenas de bytes. A descodificação é limitada (256 KiB de texto, contagens, total e valores verificados) e no máximo 4096 memórias por projeto.
- **Migração da versão 1:** um ficheiro de formato 1 abre normalmente, porque o esquema é um subconjunto do 2 (e segue a migração 2 → 3). Um ficheiro "versão 1" com componentes da versão 2 é rejeitado. Há *fixtures* gravadas com o formato 1 antes dessa mudança (`crates/bw-io/tests/fixtures/v1-*`). A única diferença de comportamento é que, no formato 1, uma entrada sem fio valia 0 e agora está flutuante (lê-se X). Os exemplos incluídos não têm entradas soltas.

### Macros (sub-circuitos hierárquicos)

Uma **macro** é um circuito reutilizável com interface própria. O desenho é original: no programa de referência as macros embutidas eram **copiadas** para dentro de cada circuito (editar o ficheiro da macro não mudava as instâncias); aqui as instâncias **referenciam** a definição.

**Definições (`bw_core::macros`).**

- Uma `MacroDef` é um circuito (o *corpo*) mais uma interface: os pinos criados pelas ***tags*** do corpo (`ComponentKind::Tag { dir: In | Out, tristate }`, um componente de 4 × 2 com um pino; a *tag* `In` conduz a rede interior a partir do pino da instância e a `Out` lê-a; uma `Out` tri-state torna essa saída da instância tri-state, que pode formar barramento), o nome, a ordem dos pinos, o modelo visual (`Package::Auto` ou `Package::Dil(n)` com o número de cada pino), um *datasheet* em texto/Markdown e, opcionalmente, um *link* para um ficheiro local.
- Uma instância é um componente `ComponentKind::Macro(MacroShape)` que **referencia** a definição pelo `MacroId` (estável). A forma (`MacroShape`: entradas, saídas, largura, máscara tri-state, pacote DIL e número de pino de cada pino) é derivada da definição e guardada no tipo, por isso a geometria dos pinos continua a ser uma função pura do tipo. No ficheiro só vai o id; a forma é recalculada ao carregar.
- O circuito raiz tem a **tabela de definições** (`MacroTable`, partilhada por `Arc`, barata de clonar). Cada entrada guarda a definição, a forma das instâncias, o ***hash* de conteúdo** (FNV-1a de 64 bits sobre nome, interface, modelo, *datasheet* e corpo; as macros aninhadas entram pelo *hash* delas e não pelo id, por isso o *hash* não depende da numeração), o tamanho achatado e a profundidade.
- **Toda** a alteração de uma definição é um `Edit::DefineMacro { id, def }` (e `Edit::RemoveMacro` para uma definição sem uso). Como as outras edições, é aplicado pela UI e pelo simulador às suas cópias. Se a interface muda, os pinos de cada instância são reassociados **pela *tag*** (o pino da *tag* X continua a ser o pino da *tag* X mesmo que mude de posição); os fios de pinos que desapareceram (ou mudaram de sentido ou de tri-state) são desligados. As definições que contêm a macro mudada são atualizadas em cascata (formas das instâncias aninhadas, *hash*, tamanho). **Editar uma definição atualiza todas as instâncias**: há testes que o verificam (comportamento, formas e fios).
- **Limites** (recusados com erro, sem mudar nada): ciclos (A contém B contém A, ou A contém A); profundidade máxima **16**; no máximo **2 000 000 componentes primitivos** no desenho completamente achatado (trava "bombas de macros" como 16 níveis × 10 instâncias; verificado ao definir, ao colocar instâncias e ao carregar); 4096 definições; até 254 entradas e 254 saídas por macro, 255 pinos ao todo, e só as primeiras 64 saídas podem ser tri-state (as peças importadas do Digital Works têm barramentos largos: um descodificador 7 → 128, uma ALU de 38 entradas); o editor de macros continua pensado para macros pequenas; nomes de pinos com até 12 bytes, nome da macro com até 64, *datasheet* com até 64 KiB.

**Criar macros.**

- **Create Macro from Selection** (menu Macro, ⇧⌘M, menu de contexto do *canvas*, botão da paleta ou das propriedades com vários componentes selecionados): os componentes selecionados passam para uma definição nova e no lugar deles fica uma instância. As redes inteiramente dentro da seleção são copiadas com a geometria; cada rede que **atravessa** a fronteira passa a ser uma *tag*: `In` se é conduzida de fora (ou não tem condutor), `Out` se é conduzida dentro (tri-state se todos os condutores de dentro o são). Os nomes vêm do pino ligado quando têm significado (`D`, `CLR`, `Q`, `QN`, `A0`…), senão `A`, `B`, … para entradas e `Y0`, `Y1`, … para saídas. A instância é ligada a cada pedaço da rede que ficou de fora. Um barramento conduzido dos dois lados da fronteira é recusado (não há pinos bidirecionais). Um teste verifica nos exemplos que o circuito se comporta exatamente como antes.
- **Seleção múltipla:** com a ferramenta Select, arrastar no vazio desenha uma **caixa de seleção** (com Shift junta à seleção) e Shift-clique junta ou retira um componente. `Del` apaga todos os selecionados. O *pan* faz-se com o scroll de dois dedos ou arrastando com o botão do meio ou o direito.
- **New Macro** cria uma definição vazia e abre-a: põem-se componentes e *tags* (paleta, secção Macros) e coloca-se depois a partir do Parts Centre.
- **Propriedades da macro** (selecionar uma instância, ou clicar no vazio dentro de uma definição): nome; ordem (botões *up*/*dn*, dentro das entradas e das saídas) e nomes dos pinos; modelo **Box** (caixa automática dimensionada pelos pinos, com os nomes dos pinos por dentro e o nome ao centro) ou **DIL 8/14/16/18/20/24/28** com o número de cada pino (lista; *Auto-assign* preenche os que faltam; os pinos do pacote sem sinal aparecem como *stubs* curtos), com **pré-visualização ao vivo**; *datasheet* e ficheiro. As alterações ficam num rascunho e aplicam-se com **Apply** (um único `DefineMacro`; escrever não recompila a simulação a cada tecla). Com mais de uma instância aparece o aviso de que a alteração afeta todas.

**Navegação na hierarquia.**

- Duplo clique numa instância (ou menu de contexto → *Edit Macro*, ou ⌘E) abre a definição **no lugar**. A barra de *breadcrumbs* mostra `root › Decade counter#3 › …` (cada nível é clicável), com **Up one level** (⌘↑) e o aviso "Editing the definition: changes affect all N instances" (N conta as instâncias em todos os níveis). Os nomes dos pinos aparecem junto das *tags*.
- Com a simulação a correr, o interior mostra o estado **dessa** instância: a UI diz ao simulador qual é o caminho de instâncias aberto (`Command::SetView`) e cada *snapshot* traz o mapa rede → pinos do circuito mostrado (componentes do corpo, saídas das macros aninhadas e as *tags* `In`, que mostram o nível que entra). O renderizador não mudou: continua a mapear redes para *slots* de sinal pelo id do componente. Uma definição aberta sem instância (*New Macro*, Parts Centre → *Open*) não tem estado ao vivo.
- Editar dentro aplica-se à definição: a edição é feita numa cópia do corpo (a cena do corpo é corrigida no próprio frame) e a definição é guardada com `DefineMacro` (todas as instâncias seguem; o simulador recompila na sua *thread*). Se for recusada (ciclo, limite), a edição é desfeita. A cena da raiz fica guardada e é corrigida pelas mudanças de nível superior (instâncias que mudaram de forma), por isso subir é imediato. Clicar num interruptor dentro de uma macro muda a definição (todas as instâncias).

**Simulação.**

- A hierarquia é **achatada na compilação** (`bw_sim::flatten`), por isso simular não tem custo de hierarquia. Cada definição (e a raiz) dá um *template* raso: as primitivas próprias, as instâncias e as redes do corpo como listas de extremos (pino de primitiva, pino de instância ou porto = *tag*). A expansão percorre as instâncias e junta as redes entre níveis com *union-find* sobre *handles* de rede (o porto de uma instância une-se à rede ligada a esse pino no nível de cima). O resultado alimenta a mesma compilação da *netlist* plana de antes.
- Cada instância tem um *scope* (o seu caminho de instâncias) com o início das suas primitivas e os *handles* dos portos: é o que serve a vista interior e as mensagens de erro. O estado dos flip-flops e das memórias passa entre compilações pelo caminho completo (instâncias + id).
- **Erros com o caminho:** uma *race condition* ou um *bus contention* dentro de uma macro dizem onde, por exemplo `Bus contention at higher level, process halted (net 7, at root › Board#4 › Clash#2 › Tri-state#3 and root › Board#4 › Clash#2 › Tri-state#4)`. O erro traz também os caminhos (`Vec<Vec<ComponentId>>`), e o contorno vermelho no *canvas* marca, em cada nível, o componente ou a instância que contém o conflito.
- **Compilação incremental:** os *templates* ficam em *cache* por definição, com o *hash* de conteúdo como chave. Editar uma definição reconstrói só o *template* dela e os das definições que a contêm (os *hashes* mudam em cascata); os outros são reutilizados. A expansão final continua a ser linear no tamanho achatado. Os tempos de cada compilação vêm no *snapshot* (`CompileStats`). Ver [Macros: desempenho](#macros-desempenho).

**Desenho.** As instâncias são desenhadas com o seu modelo (caixa ou DIL, com nome e nomes dos pinos na fonte vetorial própria, e os números de pino no DIL; num DIL o nome corre na vertical ao centro) pelo mesmo caminho com *culling* na GPU e `draw_indirect`, com um número **constante** de chamadas: um *bucket* para as *tags* e um para todas as instâncias de macros (19 *buckets* ao todo). A malha de cada definição é **acrescentada ao atlas** quando a definição aparece ou muda (`CanvasRenderer::sync_macros`: um `write_buffer` só com os vértices novos; o id da definição vai nos bits 18–31 do campo `kind` da instância e o *shader* procura `(início, n.º de vértices)` numa tabela por definição). Nada é reconstruído e não há *hitch*: se o atlas ficar pequeno cresce com uma cópia na GPU (sem reenviar o que lá está), e as malhas substituídas só são compactadas quando o lixo domina.

**Parts Centre** (menu Macro → Parts Centre, ⇧⌘P, ou o botão da paleta): janela (ou painel acoplado, opção *Docked*) com pesquisa, pastas ("This project" com as definições do projeto e quantas instâncias tem cada uma, "Built-in › …" e "My parts › …"), pré-visualização do símbolo e dos pinos (com os números DIL) e o *datasheet*, e **Place** (ou duplo clique, ou **arrastar** a peça para o *canvas*). Colocar uma peça da biblioteca importa-a com as dependências; definições com o mesmo conteúdo já presentes no projeto são reutilizadas. Uma definição importada lembra a origem (chave e *hash*): quando a versão da biblioteca muda aparece **Update from library**, que substitui a definição mantendo o id (todas as instâncias seguem). *Import…* e *Export .bwmacro…* trocam macros com ficheiros.

**Biblioteca incluída** (ficheiros `.bwmacro` em `crates/bw-io/library/`, embutidos com `include_str!`; circuitos originais feitos com as primitivas do bitwerk, cada um com um *datasheet* escrito para este projeto): meio somador; somador completo (dois meios somadores); somador de 4 bits (quatro somadores completos: três níveis); multiplexadores 2:1 e 4:1 (três 2:1); descodificador 3 → 8; registo de 4 bits (com LD e CLR); registo de deslocamento de 4 bits com carga paralela; contador síncrono de 4 bits (EN, CLR, *carry*); contador de décadas (BCD); flip-flop T; *driver* BCD → 7 segmentos; portas AND e OR de 5 e 6 entradas (árvores de portas); e quatro peças no estilo da série 74 em DIL-14: quatro NAND de 2 entradas, quatro AND de 2 entradas, seis inversores e dois flip-flops D (PRE/CLR ativos a 1, ao contrário de muitas peças reais, o que está no *datasheet*). Os ficheiros são **gerados** por código (`crates/bw-sim/tests/library.rs`, com um pequeno *router* de labirinto para os fios) e um teste falha se divergirem: `BITWERK_REGEN_LIBRARY=1 cargo test -p bw-sim --test library` regenera-os, tal como `examples/macros-demo.bitwerk`. **Todas as peças têm testes de comportamento** (tabelas de verdade completas e sequências de relógio).

**Biblioteca do utilizador:** ficheiros `.bwmacro` em `~/Library/Application Support/bitwerk/Parts` (criada quando é precisa; um subnível de pastas por pasta da biblioteca). No Parts Centre: *Add to my parts* (nas propriedades de uma macro do projeto), *Rename*, *Move* (para outra pasta) e *Delete…* (com confirmação). A leitura nunca segue *links* simbólicos, ignora entradas escondidas, tem a profundidade e o número de ficheiros limitados e trata cada ficheiro como entrada hostil; as operações só aceitam ficheiros `.bwmacro` dentro dessa pasta e nomes saneados (sem separadores, sem ponto inicial, sem caracteres de controlo).

**Datasheets e segurança.** O *datasheet* é texto/Markdown guardado na própria macro e mostrado na app. O *link* opcional para um ficheiro local só é aberto **depois de um clique explícito** em *Open datasheet file*, e só se `bw_io::datasheet::resolve_link` o aceitar:

- tem de ser um caminho de ficheiro (absoluto ou relativo à pasta do projeto) e **nunca um URL**: qualquer `esquema:` é recusado (`http:`, `file:`, `javascript:`, `mailto:` ou esquemas próprios);
- o caminho é **canonicalizado** (resolve `..` e *links* simbólicos) e tem de ser um **ficheiro regular** (um *bundle* `.app` é uma pasta e é recusado);
- a extensão do caminho canónico tem de estar na lista **pdf, html, htm, txt, md**;
- ficheiros com permissão de execução são recusados.

Só então a app corre `/usr/bin/open <caminho canónico>` diretamente, **sem *shell***. Há testes com URLs, um *script*, um `.app`, um ficheiro executável com extensão `.pdf` e um *link* simbólico `.pdf` que aponta para um *script*.

#### Macros: desempenho

Medido num MacBook Pro M1 Pro, monitor externo a 60 Hz, `--release` (todas as corridas do *sweep* válidas).

`--stress --sweep --window-pos 1260,560 --on-top` (o teste de 100k sem macros; a primeira coluna é a corrida de referência antes das macros, e as outras duas foram feitas seguidas, nas mesmas condições, com o binário de antes, compilado a partir do *commit* anterior, e com o de agora):

| varrimento inteiro | antes (1.ª corrida) | antes (A/B) | depois (A/B) |
|---|---|---|---|
| intervalo médio / p99 / máx | 16,68 / 19,05 / 35,47 ms | 16,67 / 20,35 / 22,23 ms | 16,68 / 20,55 / 33,18 ms |
| frames > 20 ms / vsyncs perdidos | 3 / 1 | 17 / 0 | 25 / 1 |
| CPU por frame média / p99 | 1,89 / 3,72 ms | 1,62 / 4,66 ms | 1,70 / 4,46 ms |
| latência edição → ecrã | 1,36 ms (0 frames) | 1,26 ms (0 frames) | 1,30 ms (0 frames) |

As diferenças estão dentro do ruído do ritmo de apresentação do macOS (o frame de 33 ms teve 5 ms de CPU). `make bench` (offscreen) fica igual: `fit` 4,70 → 4,45–4,76 ms, `2x fit` 2,49 → 2,51 ms, 4 pt/unidade 0,66 → 0,69 ms, 30 pt/unidade 0,59 → 0,61 ms, frame de edição 1,08 → 1,11 ms (1×; 2× também igual, 1,16 / 0,84 / 1,37 ms). Simulador (`stress_sim_cost`, 100k componentes sem macros): compilação + estabilização 36,7 → 28,2 ms (a compilação passa agora pelo achatamento, sem custo visível), *tick* 12,9 → 12,6 µs.

`--stress-macros --sweep` (2 500 células "contador de 4 bits → somador de 4 bits" em 50 por linha: 37 500 instâncias em quatro níveis, 70 150 primitivas; as edições do guião, nas células, incluem colocar outra instância da célula e redefinir a célula, que tem 2 500 instâncias): intervalo médio 16,68 ms, p99 20,73, máximo 33,24 (um frame), 1 vsync perdido; fase de edições p99 17,27 ms, máx 20,54; latência edição → ecrã 0,39 ms em média, máx 3,13 ms (**0 frames**).

Compilação (`cargo test --release -p bw-app -- --ignored --nocapture stress_macros_cost`, mesmo desenho, na *thread* do simulador): compilação a frio 16,5 ms (achatamento 13,0 ms, *netlist* 3,5 ms; 5 *templates* construídos), estabilização inicial 2,4 ms, *tick* 158 µs (70 000 primitivas, 10 000 flip-flops a contar). Recompilar depois de uma edição na raiz: 14,8 ms (todos os *templates* reutilizados). Redefinir o meio somador (20 000 instâncias aninhadas): modelo na UI 0,03 ms, recompilação 14,8 ms (4 *templates* reconstruídos: o dele e os das 3 definições que o contêm). Redefinir a célula com um pino novo (2 500 instâncias da raiz mudam de forma e são religadas): modelo 2,4 ms, modelo + cena 7,5 ms, no próprio frame. No *sweep*, as compilações em fundo depois de cada edição demoraram 14–25 ms, sem efeito nos frames.

### Componentes novos (formato 5)

| componente | pinos | comportamento |
|---|---|---|
| **VCC / massa** (`Const`) | uma saída | nível fixo 1 (alimentação) ou 0 (massa). |
| **Gerador de sequências** (`SeqGen`) | uma saída | padrão cíclico de 1 a 64 bits (o bit `i` do padrão é o passo `i`), um passo a cada `half_period` *ticks*, como um relógio (um relógio é o padrão `01`). *Reset* recomeça no passo 0. O Step conta cada passo como um meio ciclo. Na paleta: editor binário (passos) ou hexadecimal, com pré-visualização da forma de onda. |
| **Botão de pressão** (`Switch`, modo `Momentary`) | uma saída | com a mão, o nível é o inverso de `on` enquanto o botão do rato está premido (`on = false`: *push-to-make*; `on = true`: *push-to-break*). |
| **Entrada de impulsos** (`Switch`, modo `Pulse`) | uma saída | onda quadrada à velocidade de relógio por omissão (10 *ticks* por meio período); um clique define a fase atual (doc 5.4). |
| **Mostrador de 7 segmentos** (`SevenSeg`) | entradas a…g ao longo da base, uma unidade entre si (como no Digital Works) | cátodo comum: um segmento acende a 1; ânodo comum: a 0. |
| **Saída numérica** (`Numeric`) | 2 a 16 entradas à esquerda (a entrada `k` é o bit `k`) | binário (um algarismo por bit), hexadecimal ou decimal, com interpretação em complemento para dois opcional (doc 7.3). Um bit X/Z mostra traços. |
| **Anotação de texto** (`Text`) | nenhum | texto com várias linhas, altura, cor, negrito, itálico, barra superior e rotação de 90°. O texto fica nas `notes` do circuito; a caixa segue o texto. |

- **Cores:** LEDs, relógios, geradores, mostradores e anotações podem ter uma cor de uma paleta de 16 (`Component.color`; sem cor = a do tipo). A cor vai no campo `kind` da instância na GPU (não aumenta os 24 bytes por instância). O preto desenha-se cinzento-claro, para se ver no fundo escuro.
- **Simulação dos mostradores:** o mostrador é um nó cujos condutores extra **espelham** as entradas (nível de quatro estados da rede). O *snapshot* já leva esses "sinais de mostrador" como saídas do componente (`ComponentKind::signal_count`), por isso o renderizador acende cada segmento/algarismo pelo caminho de sempre, sem trabalho por frame.
- **Portas XOR/XNOR de 3 e 4 entradas:** já existiam (2 a 8 entradas para todas as portas). O Digital Works só tem XOR/XNOR de 2 entradas.

### Importação do Digital Works

O formato `.dwm` foi reconstruído lendo o executável do Digital Works 3.0.5 como dados (nada foi executado) e verificado nos 503 ficheiros do pacote oficial; está documentado em [`docs/dwm/DWM-FORMAT.md`](docs/dwm/DWM-FORMAT.md) e [`docs/dwm/coverage.md`](docs/dwm/coverage.md), com o *parser* Python de referência em `tools/dwm_parse.py`.

**Na app:** *File → Import Digital Works…* (⌘I) abre um `.dwm` como projeto novo (gravar pede um nome: é um *Save As*) e mostra a janela **Import report**; arrastar um `.dwm` para a janela faz o mesmo (um `.bitwerk` abre, um `.bwmacro` é importado para o projeto, uma pasta é importada para a biblioteca). No Parts Centre, *Import Digital Works folder…* importa todos os `.dwm` de uma pasta (com subpastas) para `~/Library/Application Support/bitwerk/Parts/Imported/…` numa *thread* própria, com barra de progresso e *Cancel*; as peças com o mesmo conteúdo que já lá estão são ignoradas, e os circuitos sem *tags* (que não são macros) ficam listados à parte. A janela Memory Contents carrega também os `.map` binários do Digital Works (detetados pelo número mágico `0xABCD`; truncados ou completados com zeros como no original). Na linha de comandos: `bitwerk circuito.dwm` importa no arranque (com `--run`, `--enter`, `--view`… a funcionar sobre o resultado; `--no-report` esconde o relatório) e `--import-dw-folder PASTA` corre a importação de pasta.

**Como funciona** (`crates/bw-io/src/dwm/`):

1. **Leitura** (`reader`): o *stream* de objetos do Delphi (valores com etiquetas do `TReader`, coordenadas `float32` sem etiqueta, objetos numerados antes dos campos, referências para trás) é interpretado por "programas" de leitura por classe, com uma pilha explícita e todos os limites (ver [Segurança](#segurança)). Versões 20, 30 e 34; *templates* `.dwt`.
2. **Conversão** (`convert`), por nível de macro:
   - **Componentes:** cada dispositivo passa ao componente equivalente (portas de 2–4 entradas, NOT, tri-state, flip-flops D (flanco ascendente) e JK (master-slave) com PRE/CLR, memória com o conteúdo, interruptores, relógios, geradores, VCC/massa, LEDs, 7 segmentos, saídas numéricas, *tags* e instâncias de macros) com a ordem dos pinos de cada classe. Os pinos de dados bidirecionais da memória ligam-se às entradas D **e** às saídas Q tri-state (que flutuam durante a escrita). O flip-flop RS do Digital Works não tem relógio: fica um *latch* RS com C ligado a um VCC novo.
   - **Geometria:** coordenadas ÷ 5 (a grelha do Digital Works) arredondadas à grelha. A rotação não está no ficheiro (os dispositivos rodados guardam o desenho já rodado): para cada componente procura-se a rotação e a posição que põem os nossos pinos o mais perto possível dos pinos do original (mediana das diferenças; em empate, o corpo do lado do desenho original). As entradas de uma porta são intermutáveis e são emparelhadas pela posição. Onde um pino nosso não coincide com o do original, um pequeno troço de fio (*stub*) liga-os.
   - **Fios:** cada `TWire` é um caminho de segmentos ortogonais (os poucos segmentos diagonais ganham um canto, horizontal primeiro); os pontos de solda (três fios cada) são junções; pontos repetidos fundem-se, pontas soltas são podadas. As redes são as componentes ligadas desse grafo (pinos, fios e soldas).
   - **Macros:** cada cópia embutida tem um *hash* canónico do conteúdo (o interior, sem os pinos exteriores nem o ícone, que estão nas coordenadas do pai; referências renumeradas pela ordem de visita; macros aninhadas pelo *hash* delas). Cópias iguais dão **uma** definição e as instâncias referenciam-na (por exemplo *Bus Contention*: 9 cópias → 4 definições; um teste confirma nas *fixtures* os mesmos números que o `dedup.py` da investigação). Na importação de uma pasta, as peças repetidas entre ficheiros são detetadas pelo *hash* de conteúdo do bitwerk. As *tags* passam a *tags* bitwerk com a ordem dos pinos do original; o pacote é **DIL** quando os pinos exteriores estão dispostos como um DIL (1…n/2 por um lado, n/2+1…n pelo outro, passo constante, qualquer rotação; os 7 *templates* DIL `.dwt` são reconhecidos), senão uma caixa. O nome vem do texto mais descritivo do ícone (por exemplo "74HC00"), senão do ficheiro de *datasheet*. O caminho do *datasheet* fica como **nota** no texto do *datasheet* da macro e nunca é aberto (não se cria `doc_link`). Uma macro com mais de 255 pinos (só a unidade de controlo do `Micro 5.dwm`, com 361) é colocada diretamente no circuito do pai.
   - **Texto:** os textos soltos e os nomes dos dispositivos passam a anotações (tamanho em pontos convertido para a grelha, cor, negrito, itálico, barra superior, rotação).
   - **Redes com dois condutores normais** (o Digital Works aceita-as, o bitwerk não): fica o primeiro condutor e os outros são desligados (no relatório).
   - **Relógio:** a velocidade não está no ficheiro (é uma opção global do Digital Works); os relógios e geradores importados usam 10 *ticks* por meio período, ou seja 1 Hz com a velocidade por omissão (20 *ticks*/s). O projeto importado usa o modo de simulação sequencial.
3. **Relatório** (`ImportReport`): contagens por tipo, cópias de macros → definições, e notas agrupadas em *Approximations*, *Warnings* e *Unsupported* com o número de ocorrências. Nada é descartado em silêncio.

**Não suportado** (fica no relatório): a janela Logic History (os dispositivos marcados são importados, a marcação não), desenhos livres (polígonos e linhas) e os ícones desenhados das macros (substituídos por caixa ou DIL), cores dos fios, a velocidade do relógio e o interruptor de dois terminais (`TInteractiveSwitch`, um contacto bidirecional): é aproximado por um *buffer* tri-state do lado com condutor para o outro, ativado por um interruptor novo (os pinos bidirecionais não existem no bitwerk). As *tags* bidirecionais passam a saídas tri-state.

**Resultados:** os 495 `.dwm` do pacote e dos arquivos aninhados importam sem erros (100 %) e gravam e reabrem como projeto (`BITWERK_DWM_CORPUS=<pasta> cargo test --release -p bw-io --test dwm_corpus -- --nocapture`). O maior (`Micro 5.dwm`, 5,1 MB, 408 mil objetos) importa em ≈143 ms; importar a pasta inteira para a biblioteca demora ≈1,5 s (146 peças escritas, 291 duplicados, 58 circuitos que não são macros). Testes de comportamento (`bw-sim/tests/dwm.rs`): o XOR feito com NAND (tabela completa), a igualdade de 4 bits (as 256 combinações), o JK master-slave feito com portas (SET, HOLD, TOGGLE, RESET e a saída só muda depois do flanco descendente; com atraso unitário falha, como esperado), o contador mostra 1…14 nos dois mostradores de 7 segmentos, a amostra de *bus contention* para com contenção, e os 74HC00, 74HC74, 74HC76 e 74HC125 importados como macros DIL cumprem as tabelas de verdade das *datasheets*. As *fixtures* (`crates/bw-io/tests/fixtures/dwm`, ≈570 KB) são ficheiros oficiais sem alterações, com a licença do pacote em `NOTICE`.

**Licença dos ficheiros de exemplo:** o Digital Works 3.0.5 é distribuído com uma licença permissiva do tipo ISC ("Copyright (c) 2014 by David John Barker": uso, cópia, modificação e distribuição livres, mantendo o aviso). O texto completo, tirado do tópico *License Agreement* da ajuda do pacote, está em `crates/bw-io/tests/fixtures/dwm/NOTICE` e em `fuzz/seeds/load_dwm/NOTICE`.

### Exportação para o Digital Works

*File → Save As…* (⌘⇧S) oferece no seletor de formato, além do formato bitwerk (o predefinido), **"Digital Works 3.0.5 (.dwm)"**; *File → Export as Digital Works 3.0.5 (.dwm)…* pede logo um `.dwm`. Como a conversão perde informação, é uma **exportação**: o documento continua a ser o mesmo ficheiro `.bitwerk` (⌘S grava-o, não o `.dwm`), as alterações por gravar continuam por gravar, e a barra de estado diz isso. A exportação (construção, escrita, verificação e gravação atómica) corre numa *thread* própria e no fim abre a janela **Digital Works export**: o que foi escrito, o que foi aproximado e o que ficou de fora, com contagens e *Copy as text*. Nas propriedades de uma macro, *Export as Digital Works macro…* grava uma definição como macro do Digital Works (as *tags* são a interface, os pinos pela ordem DIL). Na janela Memory Contents, *Save…* oferece o `.map` de texto do bitwerk e o `.map` binário do Digital Works.

**Como funciona** (`crates/bw-io/src/dwm/export.rs` e `writer.rs`): o exportador constrói o grafo de objetos do Digital Works (versão 34, como o Digital Works 3.x grava) e o `writer` escreve-o com **os mesmos programas de campos por classe que o leitor usa**, por isso o leitor e o escritor não podem divergir; reescrever os 503 ficheiros do pacote dá-os **byte a byte iguais**. Coordenadas × 5 em float32. Como o Digital Works não guarda rotações, as portas, NOT, tri-state, memórias, saídas numéricas, *tags* e macros levam o desenho e os pinos já rodados, exatamente sobre os nossos pinos; flip-flops, interruptores, relógios, geradores, VCC/massa, LEDs e 7 segmentos usam o símbolo próprio do Digital Works (sem rotação), colocado de forma a que os pinos coincidam com os nossos sempre que possível (senão um *stub* de fio liga-os). Os fios passam a polilinhas entre pinos e soldas; uma junção com *n* ligações passa a uma cadeia de *n* − 2 soldas de três fios (como nos ficheiros reais). As macros são copiadas para dentro de cada instância (o Digital Works não partilha definições), com os `TMacroPin` exteriores ligados aos das *tags*. Antes de entregar o ficheiro, o exportador volta a lê-lo com o importador.

**Aproximado ou perdido** (sempre no relatório): portas com mais de 4 entradas, ou XOR/XNOR com mais de 2, passam a árvores/cadeias de portas; D por flanco descendente ou master-slave leva um NOT no relógio; JK/RS por flanco passam a um D com portas de próximo estado; RS master-slave passa a *latch* RS + D; *latches* D/RS passam ao biestável RS do Digital Works com portas (e *preset*/*clear* com portas); uma RAM com entradas e saídas de dados em redes diferentes leva *buffers* tri-state ativos durante a escrita (os pinos de dados do Digital Works são bidirecionais). Perdem-se: o modo de simulação (o Digital Works propaga sempre como o modo sequencial, sem X), a velocidade dos relógios, o formato das saídas numéricas, botões de pressão (passam a interruptores), nomes dos pinos das macros (as *tags* mostram o número), o texto do *datasheet* (só o caminho do PDF sobrevive) e os *links*; texto com várias linhas é dividido em vários textos; carateres fora do Windows-1252 passam a `?`. Uma hierarquia que, copiada instância a instância, passe os 2 milhões de objetos é recusada com erro.

**Testes:** as 13 *fixtures* e os 5 exemplos exportam e voltam a importar com os mesmos componentes e redes, e comportam-se da mesma forma (simulação lado a lado com as mesmas mudanças de interruptores: XOR com NAND, igualdade de 4 bits, JK master-slave com portas, contador 1…14, 74HC00/74/76/125 exportados como macros); cada tipo de componente em cada rotação (todas as variantes de flip-flop, RAM, ROM, portas de 2 a 8 entradas) e 80 circuitos aleatórios com macros aninhadas comportam-se igual; exportar de novo um circuito já exportado dá o mesmo ficheiro byte a byte; o *parser* Python `tools/dwm_parse.py` lê tudo o que é exportado; o exportador nunca falha em 120 circuitos aleatórios (todos os tipos, rotações, macros DIL) nem no teste de carga de 20 mil componentes. Com o *corpus* completo (`BITWERK_DWM_CORPUS`, teste `whole_corpus_round_trips_through_export`): 495/495 importam, exportam e voltam a importar com a mesma estrutura, 495/495 voltam a exportar byte a byte iguais, e as 437 macros exportam como macros. O `Micro 5.dwm` (408 mil objetos) exporta em ≈125 ms. **Não verificado:** não foi possível abrir os ficheiros no Digital Works real; a confiança vem da comparação estrutural com os ficheiros reais (mesmas classes, campos, valores típicos e invariantes: soldas com três fios, contornos de pinos 8 × 8, cabeçalho igual).

### Símbolos

Cada componente tem o seu símbolo esquemático, desenhado de raiz para este projeto (não copia imagens nem *artwork* de outros produtos). Os símbolos são geometria vetorial definida em código (`crates/bw-render/src/symbols/`) e ficam **dentro do binário**: não há ficheiros externos nem *downloads*.

- **Dois estilos**: ANSI/IEEE 91 (formas distintivas, por omissão) e IEC 60617 (retângulos com `&`, `≥1`, `=1` e `1`, e bolha de inversão). Muda-se em **View → ANSI Symbols / IEC Symbols** ou nos botões ANSI/IEC da paleta (e `--style iec` no arranque). Mudar de estilo só altera um *uniform*: nenhuma malha nem instância é reenviada. O eframe não tem a *feature* de persistência ativa, por isso a escolha vale só para a sessão.
- **Componentes atuais**: AND, OR, NAND, NOR, XOR e XNOR com 2 a 8 entradas (no ANSI o lado das entradas cresce com uma linha de entrada prolongada, como nos esquemas reais; no IEC o retângulo cresce), NOT, bolhas de inversão, interruptor (alavanca aberta/fechada conforme o estado), relógio (onda quadrada, acesa quando está a 1), LED (aceso, apagado, cinzento quando flutuante, laranja quando desconhecido), *buffer* tri-state (triângulo com *enable* no topo; no IEC, caixa com `EN`, `1` e ▽), flip-flops D/RS/JK (caixa com as marcas do modo de disparo, PRE/CLR e Q/Q̄, ver [Flip-flops](#flip-flops)) e RAM/ROM (caixa com cada pino rotulado: A0…, D0…, R/W̄ e C̄S̄ com barra, Q0…, e o tipo e tamanho ao meio, por exemplo `RAM 16X4`).
- **Junção de fios:** um ponto cheio, na cor do nível da rede (como o fio), com 0,46 unidades de diâmetro (quase quatro vezes a largura do fio). Não é um componente: é desenhado automaticamente em cada junção (ver [Fios](#fios-segmentos-nós-e-junções)) e tem o seu próprio grupo na GPU (código 16).
- **Macros:** as *tags* (bandeira que aponta para fora ou para dentro, preenchida com o nível da rede; ▽ nas tri-state) e as instâncias, desenhadas com o modelo da sua definição (`symbols::macro_symbol`, ver [Macros](#macros-sub-circuitos-hierárquicos)).
- **Componentes do formato 5** (ver [Componentes novos](#componentes-novos-formato-5)): alimentação (VCC) e massa, gerador de sequências, botão de pressão e entrada de impulsos (variantes do interruptor), mostrador de 7 segmentos (cada segmento lê o seu sinal; ânodo comum por variante), saída numérica (os algarismos são calculados no *shader* a partir dos bits: binário, hexadecimal ou decimal com complemento para dois; um bit X/Z mostra traços) e anotações de texto (uma malha por anotação acrescentada ao atlas, como as das macros; negrito, itálico, barra superior e rotação). A fonte vetorial tem agora minúsculas (com descendentes) e mais sinais; os rótulos dos símbolos e os nomes das macros continuam em maiúsculas. Ao todo são 24 *buckets* de símbolos (eram 19): continua uma chamada `draw_indirect` por tipo, número constante.
- **Pinos**: os pontos de ligação de cada símbolo coincidem exatamente com a geometria de pinos do `bw-core` (`ComponentKind::pin_offset`: entradas em `(0, 2i + 1)`, saída única em `(w, h/2)`, saídas da memória em `(w, 2i + 1)`, Q/Q̄ dos flip-flops em `(w, 1)`/`(w, h − 1)`, EN do tri-state, PRE e CLR no topo/base; rotação igual a `Component::pin_pos`). Os fios terminam nos *stubs* dos pinos, que têm a mesma largura dos fios. Há testes que o verificam para todos os tipos, variantes de flip-flop, tamanhos de memória, números de entradas e estilos. Com zoom suficiente, passar o rato sobre um pino mostra o nome dele (útil nas memórias).
- **Variantes no atlas:** cada variante tem a sua malha (tipo × variante × estilo; a variante é o número de entradas das portas, o modo + PRE/CLR dos flip-flops ou o tamanho da memória, 8 bits do campo `kind`). São 22 variantes de flip-flop e 110 tamanhos de memória. Ao todo o atlas tem ≈271 mil vértices (6,3 MiB), construídos uma vez no arranque e enviados uma vez para a GPU.
- **Uma só fonte**: cada símbolo é descrito por caminhos (contorno + preenchimento) com um papel de cor por vértice. É tesselado **uma vez** (*ear clipping* para preenchimentos e faixas com *miter* para traços, feitos à mão, sem dependências como o `lyon`) para um **atlas estático partilhado** (um *array* de vértices com um sub-intervalo por tipo × número de entradas × estilo). A GPU usa esse atlas; a paleta pinta os mesmos vértices com o egui (malhas guardadas em *cache*, sem texturas); o ícone da aplicação é rasterizado a partir deles no CPU.
- **Traços nítidos em qualquer zoom**: cada vértice de traço tem um vetor de extrusão unitário que o *vertex shader* escala pela meia-largura do zoom atual (largura em unidades do mundo, mas nunca abaixo de cerca de 1 píxel físico). As etiquetas (`&`, `≥1`, Q, …) usam uma pequena fonte vetorial própria e ficam sempre direitas quando o componente roda.
- **Estado sem mudar malhas**: LED aceso, posição do interruptor, onda do relógio e cor dos pinos de saída vêm dos níveis de 2 bits por *slot* de sinal. Cada vértice tem uma camada (sempre / só a 1 / só fora de 1) que o *shader* resolve. Os *stubs* de saída levam também o índice da saída, por isso Q e Q̄, ou cada bit de dados de uma memória, mostram a sua própria rede.

### Renderização e desempenho

- **Slots estáveis.** Cada componente e cada junção têm um *slot* de corpo e cada segmento de fio um *slot* de fio. Uma edição reescreve só os *slots* que o modelo diz terem mudado (o componente, os segmentos esticados, as junções que apareceram ou desapareceram e, se as ligações mudaram, a cor das redes tocadas). Os *slots* apagados ficam escondidos e vão para uma lista livre, que os novos objetos reutilizam primeiro. A R-tree (`rstar`, usada para *picking* e etiquetas) é atualizada com `remove`/`insert`. Quando mais de metade dos *slots* está livre, a cena é compactada com uma reconstrução em fundo.
- **Uploads incrementais.** A cena regista os *slots* sujos; o renderizador junta os vizinhos em blocos e só envia esses bytes. Mover um componente envia algumas centenas de bytes, não 11 MB.
- **Carregamento em *streaming*.** Uma cena nova (ficheiro, teste de carga) é enviada para a GPU em fatias de 1 MiB por frame (≈11 frames para 100k componentes), desenhando o que já chegou. O canto superior esquerdo mostra a percentagem. Os *bind groups* cobrem só a parte já enviada, para o wgpu não ter de preencher com zeros o resto dos *buffers* novos.
- **Culling na GPU.** Um *compute shader* em três passos (contar por *workgroup*, soma de prefixos, escrever) compacta os *slots* visíveis em listas de índices e preenche os argumentos dos `draw_indirect`. Os corpos visíveis ficam agrupados por tipo de componente (17 grupos: os 16 tipos de componente e os pontos de junção, uns a seguir aos outros), o que dá ao mesmo tempo uma lista contínua para os quadrados e uma por tipo para os símbolos. Dentro de cada grupo a ordem é por *slot* e os grupos têm ordem fixa, por isso objetos sobrepostos nunca piscam. A lista é calculada para uma região com margem e reutilizada enquanto a vista lá couber e nada mudar, pelo que a maior parte dos frames de *pan* nem corre o *compute*. A CPU não faz trabalho por objeto ao navegar. Se a vista cobre a cena inteira, desenha-se tudo diretamente.
- **Símbolos e nível de detalhe.** A partir de 5 píxeis físicos por unidade de grelha (uma porta de 2 entradas tem então 20 px), cada tipo de componente é **uma** chamada `draw_indirect`. O `first_vertex` da chamada guarda o tipo nos bits altos do `vertex_index`, e cada instância procura no atlas a sua própria malha (tipo × variante × estilo) e a rotação, que estão empacotadas no campo `kind` da instância (sem aumentar os 24 bytes). Ao todo são grelha + fios + 17 chamadas (uma delas para todos os pontos de junção), seja qual for o número de objetos. Abaixo desse zoom mantém-se o LOD anterior (um quadrado por corpo, desenhado diretamente quando tudo está visível); aí os pontos de junção não são desenhados (seriam manchas de um píxel): o grupo deles fica fora da chamada dos quadrados e o *shader* dos quadrados descarta-os.
- **O *culling* é submetido à parte.** O egui-wgpu descarta o *encoder* do frame quando não obtém textura da superfície (ao abrir a janela ou com ela tapada). Por isso o passo de *compute* vai num *command buffer* próprio, submetido logo, e as listas só contam como calculadas depois disso. Antes podiam ficar vazias até à edição seguinte (via-se um *canvas* vazio em cenas pequenas).
- **Repaint a pedido.** Sem alterações, a app não desenha nada (0 % de CPU em repouso, medido com `ps`). Desenha-se quando há input, quando a câmara muda, quando o simulador publica um *snapshot*, quando chega uma cena ou enquanto se faz o *streaming*. Para medir, há `Debug → Continuous Repaint` ou `--continuous`. Com a janela tapada (*occluded*) ou minimizada, o eframe corre só a lógica e não apresenta nada, por isso nenhum vsync marca o ritmo: nesse estado os redesenhos pedidos ficam espaçados de pelo menos 250 ms, em vez de ocuparem um núcleo a 100 %.
- A cena antiga é libertada na thread `bw-scene` (libertar 460k instâncias e a R-tree custa milissegundos).

#### Medição reprodutível (`--stress --sweep`)

`bitwerk --stress --sweep [--sweep-csv frames.csv] [--window-pos X,Y]` carrega o teste de carga e, quando a cena está toda na GPU, corre um guião de cerca de 20 s: repouso com tudo visível, zoom até 40 pt/unidade, *pan*, zoom de volta, 200 edições (uma por frame, em ciclos de oito: mover um componente, rodá-lo, adicionar um NOT, ligar um ramo a meio de um segmento da rede dele (divide o segmento e cria uma junção), arrastar um segmento desse ramo, mover uma curva dele, apagar um segmento e apagar o NOT) e repouso. No fim imprime uma tabela por fase (média, p50, p95, p99 e máximo do intervalo entre frames, frames > 16,7 ms e > 20 ms, vsyncs perdidos estimados, CPU por frame e tempo de *prepare*), mais a latência edição → ecrã, e fecha. A janela fica sempre por cima durante a medição. A taxa de *refresh* é detetada pela mediana dos intervalos (arredondada à taxa comum mais próxima). Se algum frame não foi apresentado (janela tapada ou minimizada) ou se a mediana não corresponde a nenhuma taxa plausível, o resumo diz **INVALID RUN** e as estatísticas só contam os frames apresentados. Os vsyncs perdidos estimados são o tempo total da fase em períodos de *refresh* menos os frames apresentados. Ao contrário dos intervalos isolados, não contam o *jitter* de ritmo (um frame atrasado seguido de um adiantado não custa nada).

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

Com o modelo de quatro estados, tri-state, flip-flops e memória (mesma máquina, monitor externo a 60 Hz, `--stress --sweep --window-pos 1260,560`; antes = versão só com portas, níveis de 1 bit; as corridas são todas válidas):

| métrica (varrimento inteiro) | antes (1 bit) | depois (2 bits, duas execuções) |
|---|---|---|
| intervalo médio / p99 / máx | 16,68 / 18,25 / 19,30 ms | 16,68 / 17,92–18,06 / 18,45–18,60 ms |
| vsyncs perdidos (20 s) | 1 | 1 |
| CPU por frame média / p99 | 2,12 / 3,58 ms | 1,48–1,50 / 3,22–3,26 ms |
| fase de edições: *prepare* p99 | 1,74 ms | 1,87–2,01 ms |
| latência edição → ecrã | 1,53 ms (0 frames) | 1,74 ms (0 frames) |

`make bench` (offscreen): `fit` 2,8–3,7 ms (antes 2,9; varia entre execuções), `2x fit` 1,9 ms (igual), 4 pt/unidade 0,6 ms, 30 pt/unidade 0,6 ms, frame de edição 0,9 ms (iguais). Medição nova: com níveis novos em **todas** as 99 847 redes a cada frame, o *prepare* (reconversão rede → *slot* a 2 bits e envio) custa 0,27 ms.

Simulador (thread própria; `stress_sim_cost`, 100k componentes): compilação + estabilização 17 → 22 ms, um *tick* 7,9 → 9,3 µs. A avaliação a quatro estados e a resolução de barramentos custam ≈18 % por *tick*. Para o compensar, o caminho quente lê um registo compacto por nó e as redes com um só condutor não passam pela resolução. `sim_bench_flip_flops_and_memory` (512 contadores JK de 4 bits, cada um a endereçar uma RAM 16 × 8 seguida de uma árvore XOR; 6658 componentes): ≈19 000 *ticks*/s (52 µs por *tick*).

Com o grafo de fios e junções (mesma máquina). O teste de carga passou a ter fios ortogonais e *fan-out* por junções: cada porta de duas entradas recebe a segunda entrada por um ramo que sai de uma junção no fio que alimenta a porta anterior. São 100 160 componentes, 598 143 segmentos e 79 502 junções (antes ≈360k segmentos de fio, sem junções), ou seja 777 805 instâncias em vez de 459 484.

`make bench` (offscreen, MSAA 4×):

| caso | antes (459k instâncias) | depois (778k instâncias) |
|---|---|---|
| tudo visível, 1280×800 (1×) | 3,66 ms | 4,65–4,68 ms |
| tudo visível, 2560×1600 (2×) | 3,02 ms | 4,70–4,74 ms |
| 2× zoom de ajuste (1× / 2×) | 1,92 / 2,11 ms | 2,56 / 2,74 ms |
| 4 pt/unidade (1× / 2×) | 0,65 / 1,09 ms | 0,63–0,65 / 1,15 ms |
| 30 pt/unidade (1× / 2×) | 0,57 / 0,81 ms | 0,55–0,58 / 0,80–0,82 ms |
| carregamento: fatias / pior frame | 11 / 8,45 ms | 18 / 8,6 ms |
| frame de edição (mover; 1× / 2×) | 0,91 / 1,13 ms | 1,08–1,10 / 1,35 ms |
| apagar um componente: modelo | 0,58 ms (O(fios)) | 0,003 ms |
| edições de fios (dividir + ramo, arrastar, apagar): modelo + cena | — | 0,005 ms por edição |
| níveis em todas as redes a cada frame | 0,277 ms | 0,276 ms |

As diferenças com tudo visível vêm só do número de instâncias (mais 70 %); com *zoom* de trabalho os custos ficam iguais. Simulador: compilação + estabilização dos 100k 22 → 27 ms (as redes vêm agora de uma travessia do grafo de fios, ≈5 ms), um *tick* 9,3 → 9,5 µs.

`--stress --sweep` com as edições de fios (25 de cada um dos oito tipos, incluindo divisões de segmentos, junções, arrastos de segmentos, movimentos de curvas e apagamentos): as 200 edições aplicam-se todas e aparecem no próprio frame (latência edição → ecrã 0 frames). Ainda não há uma execução **válida** (com vsync) depois desta mudança: na altura o ecrã estava bloqueado, a janela não era apresentada e o resumo saiu *INVALID RUN*. A última execução válida antes da mudança era: intervalo médio 16,68 ms, p99 18,1, máximo 18,3, 1 vsync perdido, nenhum intervalo > 20 ms, edição → ecrã 1,7 ms (0 frames).

Outros números: a construção completa da cena (460k instâncias + R-tree) demora 135–150 ms em fundo. Compilação + estabilização inicial de 100k componentes no simulador: cerca de 18 ms (na thread dele). Um *tick*: microssegundos.

#### Componentes novos e importação: desempenho

Medido no mesmo MacBook (release). `make bench` (offscreen, 100k, 1× / 2×) antes → depois dos 24 *buckets*, cores e mostradores: `fit` 4,83/4,73 → 4,72/4,79 ms, `2x fit` 2,62/2,77 → 2,63/2,80 ms, 4 pt/unidade 0,69/1,19 → 0,70/1,19 ms, 30 pt/unidade 0,61/0,87 → 0,61/0,85 ms, frame de edição 1,15/1,40 → 1,18/1,44 ms, níveis em todas as redes 0,360 → 0,355 ms (iguais dentro do ruído). Simulador, binário anterior → atual: `stress_sim_cost` compilação + estabilização 29,6–30,7 → 30,7–31,1 ms, *tick* 12,4–12,6 → 12,2–12,7 µs; `stress_macros_cost` *tick* 155–158 → 160–161 µs, compilação a frio 15,4–15,7 → 15,8–16,1 ms. O `--stress --sweep` com janela ainda não foi repetido depois desta mudança: durante o trabalho o ecrã esteve bloqueado e uma corrida assim sai *INVALID RUN* (a última válida, antes: p99 19,7 ms, máx 21,1, 0 vsyncs perdidos, edição → ecrã 1,3 ms; `--stress-macros`: p99 19,4, máx 20,7, 0,39 ms).

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
- `--view X,Y,ZOOM`: posição e zoom da câmara quando a cena fica pronta (para capturas reprodutíveis);
- `--run`: começa a simular logo no arranque;
- `--tick-rate N`: velocidade da simulação no arranque (*ticks* por segundo; o mesmo que o campo *ticks/s* da barra);
- `--memory-window ID`: abre a janela Memory Contents do componente com esse id;
- `--on-top`: janela sempre por cima (como no `--sweep`), para capturas automáticas: uma janela tapada não é desenhada;
- `--stress-macros`: carrega o teste de carga com macros (2 500 células "contador de 4 bits → somador de 4 bits", quatro níveis de macros: 37 500 instâncias, 70 150 componentes primitivos); funciona com `--sweep`;
- `--parts [CHAVE]`: abre o Parts Centre (e seleciona a peça, por exemplo `builtin/dual-dff`);
- `--enter ID/ID/…`: abre esse caminho de instâncias de macros quando o projeto estiver carregado;
- `--select ID`: seleciona um componente (capturas).
- `bitwerk ficheiro.dwm`: importa um circuito do Digital Works no arranque e mostra o relatório (`--report` força-o, `--no-report` esconde-o);
- `--import-dw-folder PASTA`: importa uma pasta de macros do Digital Works para a biblioteca do utilizador (abre o Parts Centre com o progresso);
- `bitwerk FICHEIRO --export-dwm SAÍDA.dwm`: exporta um `.bitwerk` (ou um `.dwm` importado) para o Digital Works **sem abrir janela** e escreve o relatório no terminal (código de saída 0 se correu bem);
- `--save-as-dwm SAÍDA.dwm`: o mesmo dentro da app (com o ficheiro aberto) e mostra a janela do relatório.

### Utilização rápida

- **Paleta (esquerda):** escolher uma ferramenta ou componente (ícones com o símbolo; o nome aparece ao passar o rato) e clicar no *canvas* para o colocar. Enquanto se escolhe o sítio, o símbolo aparece em transparência. "Gate inputs" define o número de entradas das portas. Tri-state, D-FF, RS-FF, JK-FF, RAM e ROM entram com as predefinições da doc (D por flanco ascendente e JK master-slave, os dois com PRE/CLR; RS latch; memória 256 × 8).
- **Propriedades (direita):** tipo e modo de disparo dos flip-flops e PRE/CLR, tipo de memória, linhas de endereço e bits de dados, e *Edit contents…*. Mudar algo que altera os pinos (por exemplo o tamanho da memória) só é possível sem fios ligados.
- **Select / move:** clicar num componente seleciona-o; clicar num fio seleciona o segmento. Arrastar no vazio desenha uma caixa de seleção e Shift-clique junta/retira componentes da seleção (para *Create Macro from Selection* ou apagar vários). Duplo clique numa macro abre-a (ver [Macros](#macros-sub-circuitos-hierárquicos)); o botão direito abre um menu de contexto (Edit Macro, Create Macro from Selection, New Macro, Parts Centre, Up One Level). Arrastar um componente move-o, e os fios ligados a ele esticam-se na ortogonal (*rubber-banding*). Arrastar uma junção ou uma curva (o cursor mostra um anel azul quando há uma por baixo) move-a; arrastar um segmento desloca-o na perpendicular. Durante o arrasto vê-se uma pré-visualização; a edição aplica-se ao largar. `Del`/`Backspace` apaga o segmento ou os componentes selecionados. O *pan* faz-se com o scroll de dois dedos ou arrastando com o botão do meio ou o direito.
- **Interact:** clicar num interruptor muda o seu estado (também com a simulação parada).
- **Wire:** clicar num pino ou num fio existente para começar (num fio, o ponto mais próximo da grelha passa a ser uma junção). O troço até ao cursor é um L; `/` ou `Espaço` troca o lado da curva (horizontal ou vertical primeiro). Clicar no vazio fixa uma curva; clicar num pino, numa junção/curva ou num fio termina (num fio, cria uma junção). `Del`/`Backspace` tira a última curva; `Esc` cancela. Ligar duas saídas normais à mesma rede é recusado com uma mensagem na barra de estado. Tudo encaixa na grelha.
- **Realce da rede:** com Select ou Wire, passar o rato sobre um fio realça a rede inteira (todos os segmentos ligados).
- **Zoom:** roda do rato, *pinch* no trackpad ou Cmd + scroll, sempre centrado no cursor. **Pan:** scroll de dois dedos, arrastar com o botão do meio ou o direito, ou arrastar no vazio.
- **Teclas:** `R` roda, `Del` ou `Backspace` apaga, F9 Run, F10 Stop, F11 Pause, F12 Step; ⇧⌘M Create Macro from Selection, ⌘E Edit Macro, ⌘↑ Up One Level, ⇧⌘P Parts Centre.
- **Macros:** ver [Macros](#macros-sub-circuitos-hierárquicos). O exemplo `examples/macros-demo.bitwerk` tem um contador de décadas a alimentar um *driver* BCD → 7 segmentos com sete LEDs, um somador de 4 bits com interruptores e LEDs e uma NAND quádrupla em DIL-14 (`bitwerk examples/macros-demo.bitwerk --run`; `--enter 3` abre o contador a correr).
- **Menu Debug → Spawn Stress Test** (⇧⌘T): gera 100k componentes. O canto superior esquerdo mostra FPS, tempos de frame, o modo de desenho (tudo ou *culling* na GPU) e o progresso do *upload*. **Debug → Continuous Repaint** força o redesenho contínuo para medir.
- Há exemplos em `examples/half-adder.bitwerk` (cada entrada chega às duas portas por uma junção, com o cruzamento clássico sem ponto), `examples/all-components.bitwerk` (os tipos combinacionais, vários números de entradas e rotações; as entradas A e B correm em barras com uma junção por porta; `bitwerk examples/all-components.bitwerk --style iec` mostra o estilo IEC), `examples/flipflops-memory.bitwerk` e `examples/bus-contention.bitwerk`. O `flipflops-memory` tem um contador **síncrono** de 4 bits feito com JK master-slave: o relógio chega aos quatro flip-flops por uma única linha com uma junção por flip-flop, tal como o *clear* comum; J e K de cada flip-flop estão ligados por uma junção (J = K = 1 no primeiro, Q0 no segundo, Q0·Q1 e Q0·Q1·Q2 no terceiro e no quarto, com duas portas AND). Tem também um barramento tri-state (uma linha vertical com os dois condutores e uma junção em T para o LED) escolhido por um interruptor e um NOT (o interruptor chega aos dois por uma junção), e uma RAM de 16 × 4 endereçada pelo contador (dados de entrada, R/W̄ e C̄S̄ em interruptores; o conteúdo inicial é um padrão que se vê a andar nos LEDs). Com `bitwerk examples/flipflops-memory.bitwerk --run` o contador anda logo, e `--memory-window 18` abre a memória. Um teste do simulador abre este exemplo e confirma que conta 0, 1, …, 15, 0.

## Segurança

- `#![forbid(unsafe_code)]` em todos os *crates* (incluindo o alvo de fuzzing), mais `unsafe_code = "deny"` nos *lints* do workspace.
- Clippy `pedantic` em *warn* (e `-D warnings` no `make lint`). `unwrap_used` e `expect_used` dão aviso fora dos testes.
- `[profile.release]` com `overflow-checks = true`, `panic = "abort"`, `lto = "thin"` e `codegen-units = 1`.
- **Ficheiros = entrada hostil.**
  - O tamanho máximo (64 MiB) é verificado antes **e** durante a leitura.
  - A profundidade de recursão do RON é limitada (64).
  - Há limites de objetos (1M componentes, 4M segmentos de fio, 4M nós de fio, 1M junções, 2M fios e 256 vértices por fio nos formatos antigos, nome com 256 bytes, 4096 memórias de no máximo 16K palavras de 16 bits) e de macros: 4096 definições por ficheiro, profundidade 16, 2M componentes no desenho achatado (contra "bombas de macros"), 255 pinos por macro, *datasheet* de 64 KiB, ciclos recusados. Anotações: 1 KiB e 16 linhas, sem caracteres de controlo.
  - Os ficheiros do Digital Works são lidos por um *parser* próprio sem recursão (pilha explícita) e com limites: 64 MiB, 2M objetos, 200 mil níveis de aninhamento, 256 classes, *strings* de 1 MiB, listas de 1M itens, 1M palavras de memória (verificadas contra os bytes que faltam antes de reservar memória), coordenadas finitas, nada depois do fim. Uma classe desconhecida ou um valor com a etiqueta errada é um erro limpo. Todas as referências são verificadas pelo tipo (uma referência inválida conta como nula e fica no relatório), a profundidade das macros é limitada a 16 e os ciclos são recusados. O resultado passa pela mesma validação que um projeto.
  - O conteúdo das memórias no projeto e os ficheiros `.map` têm *parsers* próprios, limitados e testados com dados malformados (ver [Formato `.map`](#formato-map)).
  - Há validação estrutural completa antes de qualquer dado chegar à aplicação.
  - Versões futuras dão um erro explícito.
  - A gravação é atómica (ficheiro temporário + `rename`).
- **Sem rede:** nenhuma dependência de rede. O eframe é compilado sem a *feature* `links`, por isso nem sequer abre URLs. **Nada de um ficheiro é executado.**
- **Datasheets de macros:** o único sítio onde a app abre algo fora dela é o ficheiro de *datasheet* de uma macro, e só depois de um clique, só um ficheiro local regular com extensão pdf/html/htm/txt/md e sem permissão de execução, depois de canonicalizar o caminho, nunca um URL, e com `/usr/bin/open` sem *shell* (detalhes em [Macros](#macros-sub-circuitos-hierárquicos)).
- **Biblioteca do utilizador:** só ficheiros `.bwmacro` dentro de `~/Library/Application Support/bitwerk/Parts`, sem seguir *links* simbólicos, com nomes saneados.
- Dependências de fontes conhecidas, verificadas com `cargo-deny`.

### Ferramentas de segurança

```sh
cargo install --locked cargo-deny     # uma vez
make audit                            # cargo deny check: advisories, licenças, bans, fontes

cargo install --locked cargo-fuzz     # uma vez (o fuzzing precisa de nightly)
make fuzz                             # load_project, load_memory_map, load_macro, load_dwm e roundtrip_dwm, 5 min cada
make fuzz-check                       # só compila o crate de fuzzing (stable)
```

- `deny.toml` aceita só licenças permissivas (MIT, Apache-2.0, BSD, ISC, Zlib, …). A única exceção são as fontes embutidas do egui (OFL/Ubuntu Font License, que são dados). Recusa qualquer *advisory* RustSec e qualquer fonte fora do crates.io, e avisa de versões duplicadas.
- `fuzz/` é um workspace separado com os alvos `load_project` (agora também com macros: as definições voltam a ler-se com o mesmo *hash* e o tamanho achatado fica dentro do limite), `load_memory_map` e `load_macro` (`.bwmacro`: tabela válida e acíclica, a raiz usa todas as definições, volta a ler-se igual; *seeds* em `fuzz/seeds/load_macro`). Verificam que o *loader* de projetos e o *parser* de `.map` nunca entram em pânico e que tudo o que aceitam volta a ser lido igual depois de gravado (segmentos e nós incluídos). Há *seeds* do formato 3 (`fuzz/seeds/load_project/*-v3.bitwerk`). Como o *fuzzing* precisa de *nightly*, há também um teste em `stable` (`bw-io`, `mutation`) que faz 2100 mutações determinísticas dos números da parte dos fios (ids, coordenadas, pinos) dos exemplos: nenhuma pode dar pânico e tudo o que é aceite cumpre todas as invariantes do grafo e volta a ser lido igual.
- O alvo `load_dwm` alimenta o importador do Digital Works (projeto, macro, *template* e `.map`) com bytes arbitrários: nada pode dar pânico e o que é aceite cumpre as invariantes e volta a ser lido depois de gravado. As *seeds* (`fuzz/seeds/load_dwm`) são *fixtures* oficiais. Em `stable`, o teste `bw-io/tests/dwm.rs` (`mutated_fixtures_never_panic`) faz 4000 mutações determinísticas de cinco *fixtures* (bits trocados, etiquetas e inteiros alterados, pedaços apagados ou repetidos; 508 são aceites e todas são válidas e voltam a ler-se), e `hostile_streams_are_clean_errors` corta um ficheiro em todos os tamanhos e testa classes desconhecidas, versões futuras, 300 mil níveis de aninhamento e uma memória com 2³¹ palavras.
- O alvo `roundtrip_dwm` testa a exportação: qualquer *stream* que o leitor aceite é reescrito pelo `writer` e volta a ler-se como o mesmo grafo; qualquer circuito importado de um `.dwm` ou lido de um `.bitwerk` exporta sem pânico (só pode falhar por ser grande demais) e o resultado volta a importar.

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
│   ├── bw-io/            # formato de ficheiro, .map, .bwmacro, biblioteca (library/*.bwmacro), datasheets e importação do Digital Works (src/dwm; tests/fixtures: formatos 1–4 e fixtures/dwm)
│   └── bw-app/           # aplicação eframe
├── assets/               # ícone gerado por código (AppIcon.icns, AppIcon-256.png) e Info.plist
├── examples/             # half-adder, all-components, flipflops-memory, bus-contention, macros-demo
├── docs/dwm/             # formato .dwm / .dwt / .map do Digital Works e cobertura
├── tools/dwm_parse.py    # parser Python de referência (investigação)
└── fuzz/                 # cargo-fuzz
```

## Limitações conhecidas (deste esqueleto)

- O simulador recompila a *netlist* inteira a cada lote de edições (≈20 ms para 100k componentes, na thread dele). As cores das partes editadas chegam com esse atraso; a geometria aparece logo.
- Com tudo visível, a GPU desenha cada instância (≈3,5 ms com 460k num ecrã 1×). Ainda não há um LOD agregado para zoom muito afastado.
- O intervalo entre frames tem *jitter* de cerca de ±3 ms (intervalos de 19–22 ms seguidos de outros curtos), também quando a CPU e a GPU quase não trabalham. Vem do ritmo de apresentação do macOS (`nextDrawable`), não da app, e quase nunca resulta num vsync perdido.
- Ao arrancar: o primeiro frame gasta ≈28 ms de CPU (atlas de fontes do egui) e o seguinte espera ≈100 ms pela primeira apresentação da janela. Há também um frame de ≈35 ms, com 0,5 ms de CPU na UI, no instante em que a construção da cena de 100k termina em fundo.
- Fios: só as pontas ligam. Mover um componente, uma junção ou um segmento para cima de outro fio não os liga (nem os separa): as ligações só mudam ao desenhar ou apagar. Um nó de pino pode ficar por cima de outro fio sem ligação. Arrastar aplica-se ao largar o botão (a pré-visualização é um esboço do L; o resultado final é o do modelo, que pode deslizar curvas em vez de criar cantos). Mover uma curva ou junção para cima de outra do mesmo segmento funde-as.
- O realce da rede sob o cursor percorre a rede uma vez por revisão (guardado em *cache*) e desenha no máximo 4000 segmentos visíveis por frame (com o pintor do egui).
- Não há pinos bidirecionais: a RAM tem dados de entrada (D) e de saída (Q) separados. Para a ligar a um barramento, liga-se Q ao barramento e o barramento a D.
- A RAM é assíncrona e escreve por nível. Não há variante síncrona nem pino OE separado.
- Gravar o projeto guarda o conteúdo de projeto das memórias e não o que a RAM escreveu durante a simulação.
- Um JK em modo *latch* não existe (oscilaria). Um flip-flop com o relógio flutuante nunca dispara.
- O atlas de símbolos ocupa ≈6 MiB na GPU (as 110 variantes de memória com os pinos rotulados). Constrói-se uma vez no arranque.
- Ainda não há copiar/colar nem desfazer/refazer. A seleção múltipla serve para criar macros e apagar; arrastar move só o componente agarrado.
- Macros: não há pinos bidirecionais, por isso um barramento conduzido dos dois lados não pode atravessar a fronteira de uma macro. A vista interior mostra o estado de uma instância, mas clicar num interruptor lá dentro muda a definição (todas as instâncias). Uma definição aberta sem instância não mostra estado. Editar dentro de uma macro clona o corpo da definição a cada edição (barato para macros de tamanho normal; um corpo com dezenas de milhares de componentes tornaria cada edição lenta). O *router* da biblioteca existe só no gerador (os fios criados por *Create Macro from Selection* são Ls simples e podem cruzar componentes). A pré-visualização do Parts Centre não mostra o estilo IEC (as macros só têm um estilo).
- Com a simulação parada, o circuito combinacional continua a ser avaliado depois de cada edição, o que dá feedback imediato. O tempo (relógios) só avança com Run ou Step.
- O menu nativo só existe em macOS. Noutras plataformas, a barra egui mostra Open/Save.
- Símbolos: os *stubs* dos pinos de entrada têm a cor do contorno do componente, e não a do nível do fio, porque a instância só aponta para os sinais das suas saídas. O fio que chega ao pino tem a cor certa. Num barramento, todos os segmentos mostram o nível resolvido da rede (o mesmo para todos os condutores). Uma rede sem condutor desenha-se como flutuante. A escolha ANSI/IEC não é gravada entre sessões. Os rótulos dos símbolos e os nomes das macros aparecem em maiúsculas (as anotações têm minúsculas). As malhas das anotações são construídas de uma vez quando a cena aparece (2000 anotações: um frame de ≈18 ms); depois só a anotação editada é reconstruída.
