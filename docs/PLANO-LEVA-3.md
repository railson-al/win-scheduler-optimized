# Plano de desenvolvimento: Leva 3 (interface gráfica)

Contexto: motor, IPC local e serviço do Windows (levas 0 a 2) estão prontos e commitados. A CLI `scriptrunner` já é
cliente do daemon por IPC. Falta a GUI: um painel para cadastrar, agendar, executar e monitorar scripts sem
digitar comandos. A GUI é **só mais um cliente** do daemon, usando o mesmo `scriptrunner-ipc::client::Client`.

Legenda de responsável:
- **Haiku**: subtarefa mecânica ou de pesquisa, com especificação fechada. Eu reviso antes de integrar.
- **Sonnet**: desenho, concorrência, `unsafe` e integração, onde um erro sutil custa caro.

Tamanhos: S (menos de 1 hora), M (algumas horas), L (um dia ou mais).

---

## Decisões

Tomadas pelo usuário:

- **Escopo completo na v1:** painel de tarefas, criar e editar tarefa com agenda, histórico, visualizador de log ao
  vivo e configurações. Tudo o que o protocolo e a CLI já oferecem.
- **Sem ícone de bandeja na v1.** Janela comum. O serviço já roda sem sessão aberta, então a GUI é um painel que se
  abre quando preciso.
- **Binário separado** (`scriptrunner-gui`), sem SQLite, falando só com o crate `ipc`. O `scriptrunner.exe` e o
  serviço não carregam dependência gráfica.
- **Idioma:** pt-BR, com os textos numa tabela central, para acrescentar inglês depois sem refatorar.

Já decidida antes e que vale aqui (D6 da leva 2): o pipe só aceita SYSTEM e Administradores, então **a GUI exige
elevação (UAC)**. Nada de afrouxar o pipe para usuário comum, pois quem alcança o pipe executa código como SYSTEM.

Assumidas por mim (me avise se discordar):

- **D13. Framework: `egui` com `eframe` 0.37.0** (confirmado na 3.1). Ela exige Rust 1.95 e o workspace declara 1.88,
  então o crate `gui` declara o próprio `rust-version = "1.95"`. A toolchain fixada (1.99.1) cobre isso; o mínimo só
  importa a quem compilar com um compilador mais antigo. A 0.33.3 caberia nos 1.88, mas o Cargo só a escolhia por
  causa do filtro de MSRV. O `rfd` 0.18 (seletor de arquivo da 3.6) exige 1.92, o que também cabe.
- **D14. Backend gráfico: decidir num teste em máquina Windows sem GPU (tarefa 3.1).** O OpenGL que a `glow` exige
  pode não existir numa sessão RDP ou numa VM de servidor, que é justamente onde um agendador roda. A alternativa é
  `wgpu` (DX12 com fallback de software). É um risco que conheço de memória e não verifiquei; só dá para confirmar no
  Windows, então a 3.1 é um *spike* com critério de saída: abrir a janela por RDP.
- **D15. Elevação por manifesto:** o `.exe` da GUI embute um manifesto `requestedExecutionLevel =
  requireAdministrator`, para o Windows pedir UAC ao abrir. Sem isso, o usuário vê o erro de acesso negado do pipe,
  que a CLI já traduz para "execute como Administrador". No Linux (desenvolvimento) não há manifesto.
- **D16. Threads:** a UI do egui é síncrona e roda na thread principal. Um runtime tokio vive em outra thread e
  segura o `Client`. A UI manda comandos por um canal; o backend devolve resultados e eventos por outro, e chama
  `ctx.request_repaint()` ao receber algo. A UI **nunca espera** uma chamada IPC.
- **D17. A tela mostra o que o daemon diz, sem estado próprio duplicado.** Depois de `Subscribe`, cada
  `TasksChanged` ou `ExecutionChanged` atualiza a lista; `Resync` ou `Lagged` refazem a consulta inteira, como o
  contrato do cliente manda. Se a conexão cai, a GUI entra num laço de reconexão com recuo (1 s até 10 s) e mostra
  "daemon parado" com o motivo, sem fechar.
- **D18. Horários em local.** O daemon fala UTC; a GUI converte para o fuso da máquina na exibição (`chrono::Local`).
  A prévia das próximas execuções usa `Schedule::next_after` do `core`, sem pedir nada ao daemon.
- **D19. Lógica fora do egui.** Estado, filtros, validação do formulário e formatação ficam em módulos puros,
  testáveis sem janela. As telas só desenham.
- **D20. Caminho dos dados compartilhado.** A função que resolve a pasta de dados está hoje privada em
  `service/src/main.rs`. Vai para o `core` (ou `ipc`), para a CLI e a GUI usarem a mesma.

---

## Pré-requisitos no protocolo (leva 3.0)

`PLANO-LEVA-0-1.md` já dizia que P1, P2, P3 e P6 devem ser resolvidas **antes** da GUI. Cada uma bloqueia uma tela:

| # | Pendência | Por que bloqueia a GUI | Proposta |
|---|---|---|---|
| P1 | `ReadLog` relê o arquivo desde o início a cada página, e `ExportLog` falha acima de ~8 MiB | O log ao vivo faria consultas repetidas a cada meio segundo, cada vez mais caras, e exportar um log grande não funciona | Novo `ReadLogFrom { id, offset, max_bytes }` que devolve `LogChunk { entries, next_offset, eof }`. Exportar passa a baixar em pedaços e a GUI grava no arquivo. O formato do arquivo de log é conferido na 3.0 |
| P2 | Versão exata no handshake; variante desconhecida derruba a conexão | GUI e daemon podem ficar em versões diferentes depois de uma atualização parcial | Aceitar faixa de versões (`min` e `max`) e responder `Unsupported` a requisição desconhecida, em vez de fechar |
| P3 | O servidor atende uma requisição por vez por conexão | Uma exportação lenta travaria os botões Parar e Executar | Documentar resposta em ordem e dar à GUI **duas conexões** (uma para comandos, outra para leitura de log), ou paralelizar com limite no servidor. Recomendo a segunda, que não mexe no servidor |
| P6 | `usize` no fio (`ReadLog`, `Stopped`); sem timeout de ociosidade | `usize` muda de tamanho entre 32 e 64 bits e vira contrato da GUI | Trocar por `u32` e `u64` já na mudança de P1 |

Como tudo isso muda `PROTOCOL_VERSION`, vai **junto, numa única subida** do número (de 1 para 2).

---

## Estrutura

```
crates/gui/                      novo crate (bin: scriptrunner-gui)
  Cargo.toml                     eframe, tokio, scriptrunner-core, scriptrunner-ipc, rfd; sem rusqlite
  build.rs                       cfg(windows): embute o manifesto requireAdministrator
  src/main.rs                    janela, runtime tokio, ligação UI <-> backend
  src/backend.rs                 thread do tokio: Client, reconexão, comandos e eventos
  src/state.rs                   estado da aplicação e reducers (puro, sem egui)
  src/form.rs                    modelo e validação do formulário de tarefa e de agenda (puro)
  src/fmt.rs                     duração, data local, rótulo e cor de status (puro)
  src/i18n.rs                    tabela central de textos (pt-BR)
  src/views/dashboard.rs         painel de tarefas
  src/views/task_editor.rs       criar e editar tarefa e agenda
  src/views/history.rs           histórico com filtros e paginação
  src/views/log_view.rs          detalhe da execução e log ao vivo
  src/views/settings.rs          configurações
  src/views/status_bar.rs        conexão, versão e uptime do daemon
```

Mudanças em outros crates:

- `ipc`: `ReadLogFrom`, faixa de versões, tipos de largura fixa (leva 3.0).
- `core` (ou `ipc`): função da pasta de dados compartilhada (D20).
- `service/src/main.rs`: passa a usar a função compartilhada.
- `.github/workflows/ci.yml`: compilar e testar o crate da GUI nas duas plataformas.

## Telas

1. **Painel.** Tabela com nome, resumo da agenda, próxima execução, último resultado e indicador de "rodando".
   Ações por linha: executar agora, parar, pausar ou retomar a agenda, editar, duplicar, excluir (com confirmação).
   Busca por nome. Dados de `Overview`, atualizados por eventos.
2. **Editor de tarefa.** Nome, descrição, script (seletor de arquivo; o tipo sai da extensão), argumentos, interpretador,
   pasta de trabalho, timeout, "permitir execuções simultâneas" e "guardar logs". Editor de agenda para as quatro formas
   (intervalo, diário, semanal, cron) com validação em tempo real e as **cinco próximas execuções** na tela.
3. **Histórico.** Filtros por tarefa, status e período; paginação por `limit` e `offset`; "limpar histórico da tarefa".
4. **Execução e log.** Cabeçalho (status, código de saída, duração, motivo da falha) e o log, com cores por saída
   (stdout e stderr), filtro por stream, rolagem automática, copiar e exportar para arquivo. Acompanha uma execução em
   andamento por `ReadLogFrom`.
5. **Configurações.** Todos os campos de `Settings`, com validação (`max_concurrent_executions >= 1`). O limite de
   concorrência vale na hora, sem reiniciar o daemon.
6. **Barra de status.** Estado da conexão ("conectado", "daemon parado", "reconectando"), versão e uptime do daemon.

## Tarefas

| # | Tarefa | Quem | Tam. | Depende de |
|---|---|---|---|---|
| 3.0 | Protocolo v2: `ReadLogFrom`, faixa de versões, `Unsupported`, `u32` e `u64` no fio (P1, P2, P6), testes de integração e migração da CLI (`logs`) para o novo pedido | Sonnet | L | |
| 3.1 | **Spike no Windows:** crate `gui` mínimo que abre uma janela com `glow` e depois com `wgpu`, em máquina sem GPU e por RDP. Confirma o MSRV da versão do eframe. Saída: backend escolhido e registrado em D14 | Sonnet (código), usuário (rodar no Windows) | M | |
| 3.2 | Esqueleto do crate `gui`, `i18n.rs`, `fmt.rs`, função da pasta de dados compartilhada (D20) e manifesto de elevação (`build.rs`) | Haiku, com especificação | M | 3.1 |
| 3.3 | `backend.rs`: thread do tokio, `Client`, `Subscribe`, reconexão com recuo, `Resync`, duas conexões (P3), canais de comando e de evento | Sonnet | L | 3.0, 3.2 |
| 3.4 | `state.rs`: estado e reducers para tarefas, execuções, conexão e erros, com testes sem janela | Sonnet | M | 3.3 |
| 3.5 | Painel e barra de status | Haiku, com a API do estado pronta | M | 3.4 |
| 3.6 | `form.rs` (validação e conversão para `Task` e `Schedule`) e editor de tarefa, com prévia das próximas execuções | Sonnet (formulário e agenda), Haiku (widgets) | L | 3.4 |
| 3.7 | Histórico com filtros e paginação | Haiku, com especificação | M | 3.4 |
| 3.8 | Detalhe da execução e visualizador de log ao vivo (`ReadLogFrom`, rolagem, filtro, exportar) | Sonnet | L | 3.3, 3.4 |
| 3.9 | Configurações | Haiku, com especificação | S | 3.4 |
| 3.10 | Testes de interface com `egui_kittest` (**confirmar que existe para a versão escolhida**): painel, editor com erro de validação, fluxo de executar e ver o log | Sonnet | M | 3.5 a 3.9 |
| 3.11 | CI: compilar e testar o `gui` em ubuntu e windows; no Windows, conferir que o `.exe` embute o manifesto de administrador | Haiku, com especificação | S | 3.2 |
| 3.12 | README: como abrir a GUI, elevação, capturas de tela e o checklist manual abaixo | Haiku | S | 3.9 |
| 3.13 | Revisão final com `rust-reviewer` e `security-reviewer` sobre o diff inteiro | Sonnet | S | todas |

### Paralelização

A 3.0 e a 3.1 são independentes (`ipc` e `gui`), então rodam em paralelo. A 3.2 espera a 3.1. Depois da 3.4, as telas
3.5, 3.7 e 3.9 (Haiku) rodam em paralelo com a 3.6 e a 3.8 (Sonnet): cada uma mexe num arquivo de `views/`. Nunca dois
executores no mesmo arquivo. A 3.4 define a API que as telas consomem, então o estado fecha antes das telas começarem.

---

## Aceite da leva 3

**Automático**
- Linux: testes do estado, do formulário e da formatação (sem janela), testes de integração do protocolo v2 e testes
  de interface com `egui_kittest`, com `clippy -D warnings` e `fmt` limpos.
- CI em ubuntu e windows: o crate `gui` compila e testa. No Windows o job confere o manifesto no `.exe`.
- O crate `gui` **não** puxa `rusqlite` (conferir com `cargo tree -p scriptrunner-gui`).

**Manual no Windows (só você consegue confirmar)**
1. Abrir `scriptrunner-gui.exe` por duplo clique: o UAC aparece e, depois de aceitar, a janela abre.
2. A janela também abre numa sessão RDP e numa VM sem GPU (valida a decisão D14).
3. Com o serviço parado, a barra mostra "daemon parado"; ao iniciar o serviço, a GUI reconecta sozinha e a lista aparece.
4. Criar uma tarefa `.bat` com caminho com espaços e agenda de 1 minuto: aparece no painel, dispara sozinha, e o log
   mostra os acentos corretamente.
5. Executar agora e acompanhar o log ao vivo: as linhas chegam enquanto o script roda, sem travar a janela.
6. Parar uma execução longa pela GUI: o status vira `interrupted`.
7. Abrir a GUI duas vezes: as duas recebem os eventos de uma execução disparada por uma delas.
8. Reiniciar o serviço com a GUI aberta: ela recupera a conexão e refaz a lista (`Resync`).
9. Exportar o log de uma execução grande (mais de 8 MiB) para arquivo, completo.
10. Mudar o limite de concorrência em Configurações e confirmar que vale sem reiniciar.
11. DPI alto (150% e 200%) e tela secundária: texto legível, nada cortado.

---

## Riscos

| Risco | Mitigação |
|---|---|
| Nada da GUI roda no Linux como no Windows; o backend gráfico pode não abrir em RDP ou VM sem GPU | Spike 3.1 antes de investir nas telas; lógica pura testada no Linux; aceite manual 2 |
| Elevação por manifesto: o usuário não administrador vê o UAC pedir credenciais | É o efeito esperado de D6. A GUI não tenta contornar o pipe |
| UI travar esperando o daemon | D16: nenhuma chamada IPC na thread da UI; testes de que a UI continua respondendo com o daemon parado |
| Log ao vivo caro ou com perda de linhas | `ReadLogFrom` com deslocamento em bytes (3.0) e teste com script que imprime muito |
| GUI e daemon em versões diferentes | Faixa de versões e `Unsupported` (3.0); a barra de status mostra a versão do daemon |
| Formulário de agenda aceitar o que o daemon recusa | `form.rs` reusa `Schedule::validate` e `Task::validate` do `core`, as mesmas regras do daemon |
| Mudança de protocolo quebrar a CLI | A 3.0 migra a CLI no mesmo trabalho e os testes de integração cobrem os dois clientes |
| Versão do eframe pedir Rust mais novo que o mínimo do projeto | Confirmar o MSRV na 3.1; se for preciso, subir `rust-version` do workspace de forma consciente e registrada |

## Pontos a decidir (nenhum bloqueia o início)

| # | Ponto | Observação |
|---|---|---|
| R1 | Botão "iniciar serviço" na barra quando o daemon está parado | A GUI já roda elevada, então poderia chamar `scriptrunner-winsvc`. Fica fora da v1 para não ampliar o código de Windows a validar |
| R2 | `Task::duplicate` grava `"(cópia)"` fixo no `core` | Com a tabela de textos da GUI, o certo seria a GUI nomear a cópia. Hoje funciona em pt-BR; decidir ao acrescentar um segundo idioma |
| R3 | Nomes de tarefa repetidos | O painel trabalha por id, mas a CLI escolhe o primeiro por nome. Decidir se o nome deve ser único (não verifiquei se o banco impede) |
| R4 | Q1 a Q6 da leva 2 | Continuam pendentes; Q5 (testes de transporte com o serviço instalado) atrapalha quem desenvolve a GUI no Windows |

## Estado

### 3.0 Protocolo v2: pronta no Linux

Verificado: 118 testes passando (10 novos: 3 no `core`, 1 no `ipc`, 6 de integração), `clippy -D warnings` no Linux e no
alvo Windows, `fmt` limpo, seis rodadas dos testes de integração sem falha e um teste de ponta a ponta com o binário
real (`run` acompanhando ao vivo e `logs` de um log de 3 MB). **Não verificado no Windows**, como todo o transporte
por pipe.

O que mudou em relação ao desenho acima:

- **Versões:** o cliente manda a mais nova que fala e o daemon responde com a **menor das duas**; só recusa um cliente
  mais velho que `MIN_PROTOCOL_VERSION` (1). Um daemon v1 continua recusando um cliente v2 (ele exige versão exata e
  não dá para mudá-lo): a GUI precisa de um daemon atualizado junto. O cliente expõe `protocol_version()` e
  `read_log_from` devolve `Unsupported` contra um daemon v1.
- **Tipos desconhecidos:** `#[serde(other)]` não serve para estes enums (rejeita a variante que ainda traz `params`
  ou `data`), então `Request`, `Response` e `Event` usam um desserializador tolerante (`or_unknown`) e
  `ErrorCode` usa `other`. Efeito colateral: uma requisição conhecida com parâmetros malformados também vira
  `Unknown` e recebe `Unsupported`, não um `Fatal` de "frame malformado".
- **P1:** `ReadLogFrom` devolve `LogChunk` (entradas, `next_offset`, `eof`), lê só linhas completas e deixa uma linha
  ainda sendo escrita para a próxima leitura. `Client::export_log` agora busca página por página (1 MiB) e não falha
  mais acima de 8 MiB; o pedido antigo `ExportLog` continua no daemon para clientes v1 e ainda dá `TooLarge`.
  `export_log` monta o texto inteiro em memória.
- **P3:** ficou a opção documentada. O daemon responde **em ordem** por conexão (comentário em `protocol.rs`) e a GUI
  usa duas conexões (3.3); nada mudou no servidor.
- **P6:** `usize` virou `u32` e `u64` no fio. **O timeout de ociosidade não foi feito** de propósito: uma GUI
  inscrita nos eventos fica ociosa por horas de forma legítima, e o pipe só aceita administradores.
- **P7 (já conhecida):** o caminho do socket Unix acima de ~108 bytes atrapalhou o teste de ponta a ponta no
  scratchpad. Continua pendente e só afeta desenvolvimento no Linux com `SCRIPTRUNNER_DATA_DIR` longo.
- Pequeno: `print!` da CLI entra em pânico com `Broken pipe` quando a saída é cortada (`scriptrunner logs ... | head`).
  Já existia; não mexi.

### 3.1 Spike do backend gráfico: código pronto, falta rodar no Windows

O crate `crates/gui` abre uma janela com o renderer pedido (`wgpu` por padrão, ou `glow`) e mostra qual adaptador
e qual API gráfica ele realmente obteve, também no console. Compila e passa no clippy no Linux e no alvo Windows, e não
puxa SQLite (`cargo tree`). No Linux, com GPU, os dois renderers abriram (`wgpu` por Vulkan na Iris Xe, `glow` com
OpenGL 4.6), o que **não responde** à dúvida do RDP.

**O que só você consegue fazer, numa máquina Windows sem GPU (VM de servidor, ou sessão RDP):**

```powershell
cargo build -p scriptrunner-gui
.\target\debug\scriptrunner-gui.exe wgpu
.\target\debug\scriptrunner-gui.exe glow
```

Anote para cada um: se a janela abre e desenha, o texto do relatório (adaptador, backend, tipo de dispositivo) ou a
mensagem de erro do console. Critério de decisão para D14:

| `wgpu` | `glow` | Decisão |
|---|---|---|
| abre | abre | `wgpu`: é o padrão do eframe 0.37 e tem fallback de software |
| abre | falha | `wgpu` (confirma o risco do OpenGL no RDP) |
| falha | abre | `glow` |
| falha | falha | parar e discutir (renderização por software ou outra biblioteca) |

Depois da decisão, o `Cargo.toml` do `gui` passa a ter só o renderer escolhido.

## Fora desta leva

Ícone de bandeja e notificações, instalador MSI, "executar como usuário", tema escuro e atalhos extras, importar e
exportar tarefas, tradução para inglês (a estrutura fica pronta), benchmarks (fase 4) e Event Log.
