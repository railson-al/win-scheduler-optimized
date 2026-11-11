# Plano de desenvolvimento: Leva 0 (preparação) e Leva 1 (IPC)

Contexto: o motor (Fase 1) está pronto e commitado. Falta serviço Windows, IPC e GUI. Este plano cobre
as duas primeiras levas. A leva 2 (serviço Windows) e a leva 3 (GUI) ficam para depois.

Legenda de responsável:
- **Haiku**: subtarefa mecânica ou de pesquisa, com especificação fechada. Eu reviso antes de integrar.
- **Sonnet**: desenho, concorrência e integração, onde um erro sutil custa caro.

Tamanhos: S (menos de 1 hora), M (algumas horas), L (um dia ou mais).

---

## Leva 0: preparação

Objetivo: o repositório passa a se verificar sozinho (CI em Linux e Windows) e o motor deixa de bloquear
o runtime async, antes de o IPC aumentar a concorrência.

| # | Tarefa | Quem | Tam. | Arquivos |
|---|---|---|---|---|
| 0.1 | `rust-toolchain.toml` com `channel = "1.99.1"` (versão local, fixada para o `clippy -D warnings` ser reproduzível) e componentes `rustfmt` e `clippy`; `rust-version = "1.88"` em `[workspace.package]` (let-chains só estáveis a partir da 1.88, e só na edition 2024) | Haiku | S | `rust-toolchain.toml`, `Cargo.toml` |
| 0.2 | `rustfmt.toml` com `max_width = 120` (o código já segue 120, mas não de forma consistente) e um único `cargo fmt --all`. **O `fmt --check` falha hoje**: 146 trechos com a largura padrão e 93 com 120, então a reformatação é inevitável | Haiku | S | `rustfmt.toml`, todos os `.rs` |
| 0.3 | Workflow de CI: matriz ubuntu e windows com fmt, clippy `-D warnings` e test. Rascunho pronto e revisado, com `actions/checkout@v7.0.1` e `Swatinem/rust-cache@v2.9.3`; o Haiku só o copia para `.github/workflows/ci.yml`. As versões das ações vieram do agente de pesquisa e serão confirmadas pela primeira execução do CI | Haiku | S | `.github/workflows/ci.yml` |
| 0.4 | `LICENSE` (MIT, já declarada no `Cargo.toml`) | Haiku | S | `LICENSE` |
| 0.5 | Chamadas SQLite e `std::fs` do `Engine` em `spawn_blocking` | Sonnet | M | `crates/service/src/engine.rs` |
| 0.6 | README: seção de CI e de versão do Rust, e link para este plano | Haiku | S | `README.md` |

Ordem: 0.1 e 0.2 primeiro (0.3 depende deles). 0.2 reformata muitos arquivos, então vai em **commit
separado e sozinho**, para não poluir o diff das outras tarefas. 0.4 e 0.6 são independentes.

### 0.5 em detalhe

- Hoje `fire_due`, `housekeeping`, `persist`, `load_settings` e `remove_log_files` fazem E/S síncrona
  dentro de tarefas async.
- Criar um helper `Engine::blocking(f)` que clona o `Engine` e roda `f` em `tokio::task::spawn_blocking`.
- Aplicar em `run_scheduler` (passada de `fire_due` e `housekeeping`) e em `run_execution` (`load_settings`,
  `persist`, `finish`).
- Os métodos públicos síncronos (`create_task`, `list_tasks` etc.) **não mudam de assinatura**. O servidor
  IPC da leva 1 vai chamá-los via `blocking`.
- Aceite: os 12 testes do service continuam passando sem alteração de comportamento. Um teste novo mostra que
  o scheduler continua disparando enquanto o banco está ocupado por outra conexão.

### Aceite da leva 0

- CI verde em ubuntu e windows no primeiro push. O job do Windows é a **primeira execução automatizada dos
  testes em Windows**, e pode revelar falhas reais: os testes de executor e engine hoje são `cfg(unix)` e
  não rodam lá.
- `cargo fmt --check`, `clippy -D warnings` e `test` limpos localmente.

---

## Leva 1: IPC, com o daemon em modo console

Objetivo: o `daemon` passa a escutar em um canal local, e a CLI vira cliente dele. Isso elimina o problema
de a CLI e o daemon serem processos separados com trava de sobreposição só em memória. A GUI da leva 3 usa
exatamente o mesmo cliente.

### Decisões assumidas (me avise se discordar)

- **D1. Transporte:** named pipe no Windows (`\\.\pipe\scriptrunner`) e socket Unix em `<data_dir>/scriptrunner.sock`
  no Linux. Abstração fina com `cfg`, escrita por nós.
  *(Confirmar contra a pesquisa sobre o crate `interprocess`.)*
- **D2. Enquadramento:** frames com prefixo de tamanho (u32), corpo em JSON, limite de 8 MiB por frame.
- **D3. CLI sem modo direto:** todos os comandos passam pelo IPC. Se o daemon não estiver rodando, erro claro.
  O acesso direto ao banco some, o que remove o risco do `startup` interromper execuções de outro processo.
- **D4. Segurança nesta leva:** socket Unix num diretório 0700; pipe com `reject_remote_clients`. As ACLs
  completas do pipe e de `%PROGRAMDATA%` ficam na leva 2, junto com a decisão da conta do serviço.

### Estrutura

Novo crate `crates/ipc` (`scriptrunner-ipc`), que depende só de `core`:

```
crates/ipc/src/
  protocol.rs   Request, Response, Event, ErrorBody, versão do protocolo
  codec.rs      leitura e escrita de frames JSON
  transport.rs  Listener e Stream (named pipe no Windows, Unix no Linux)
  client.rs     Client: chamadas request/response e assinatura de eventos
```

O servidor fica em `crates/service/src/ipc_server.rs`, mapeando cada requisição para um método do `Engine`.

### Contrato do protocolo (resumo)

- Primeiro frame: `Hello { protocol_version }`. Versão incompatível fecha a conexão com erro explícito.
- Cliente envia `{ id, request }`. O servidor responde `{ id, result }` ou envia `Event` não solicitado.
- Requisições (espelham a API pública do `Engine`): `Status`, `Overview`, `ListTasks`, `GetTask`, `CreateTask`,
  `UpdateTask`, `DeleteTask`, `DuplicateTask`, `SetScheduleEnabled`, `RunNow`, `Stop`, `Executions(query)`,
  `GetExecution`, `ClearHistory`, `ReadLog`, `ExportLog`, `GetSettings`, `UpdateSettings`, `Subscribe`.
- Erros: `ErrorBody { code: NotFound | AlreadyRunning | Invalid | Internal, message }`. Nunca vaza caminho
  interno nem stack.
- Eventos: `TasksChanged` e `ExecutionChanged(Execution)`, só depois de `Subscribe`.

### Mudanças de tipos necessárias (em `core`)

- `ExecutionQuery` sai de `storage` para `core::models`, com `Serialize` e `Deserialize`. O `storage` o
  reexporta, então nenhum chamador quebra. Assim o `ipc` não depende do `storage`, e a GUI não carrega SQLite.
- `TaskOverview` sai de `service::engine` para `core::models`, também com serde.
- `LogEntry` e `LogKind` (em `core::logs`) ganham `Serialize` e `Deserialize`.

### Tarefas

| # | Tarefa | Quem | Tam. | Depende de |
|---|---|---|---|---|
| 1.1 | Mover `ExecutionQuery` e `TaskOverview` para `core`; serde em `LogEntry` e `LogKind`; ajustar imports | Sonnet | S | leva 0 |
| 1.2 | Scaffold do crate `ipc` e `protocol.rs` (tipos e erros) | Sonnet | M | 1.1 |
| 1.3 | `codec.rs`: ler e escrever frames JSON, limite de tamanho, testes de ida e volta, frame grande, frame truncado, JSON inválido | Haiku, com especificação | S | 1.2 |
| 1.4 | `transport.rs`: Listener e Stream por plataforma; no Unix, socket em diretório 0700 com remoção de socket obsoleto só se ninguém responder; no Windows, laço de accept do pipe com `first_pipe_instance` e retry de `ERROR_PIPE_BUSY` no cliente (detalhes na seção de pesquisa) | Sonnet | M | 1.2 |
| 1.5 | `ipc_server.rs`: handshake, despacho para o `Engine` via `blocking`, assinatura e envio de eventos, desligamento limpo, limite de conexões | Sonnet | L | 1.3, 1.4, 0.5 |
| 1.6 | `client.rs`: conexão, correlação por `id`, fluxo de eventos, erro claro se o daemon não está rodando | Sonnet | M | 1.3, 1.4 |
| 1.7 | `daemon` sobe o servidor IPC junto do scheduler | Sonnet | S | 1.5 |
| 1.8 | Migrar os comandos da CLI (`add`, `list`, `run`, `enable`, `disable`, `delete`, `history`, `logs`) para o `Client`; `logs` passa a usar `GetExecution` em vez de carregar 10.000 execuções | Haiku, com a API do `Client` pronta | M | 1.6 |
| 1.9 | Testes de integração: daemon real em diretório temporário, cliente real; cobertura de cada requisição, eventos, dois clientes simultâneos, erro de handshake | Sonnet (cenários principais) e Haiku (casos de erro a partir da especificação) | M | 1.7 |
| 1.10 | README: novo fluxo `daemon` mais CLI como cliente, e o checklist de validação no Windows | Haiku | S | 1.8 |
| 1.11 | Revisão final com o agente `rust-reviewer` sobre o diff inteiro | Sonnet | S | todas |

### Paralelização

Depois de 1.2, as tarefas 1.3 (Haiku) e 1.4 (Sonnet) rodam em paralelo, pois mexem em arquivos distintos.
Depois de 1.5 e 1.6, a 1.8 (Haiku) roda enquanto escrevo a 1.9. Nunca dois executores no mesmo arquivo.

### Aceite da leva 1

- `scriptrunner daemon` num terminal e `scriptrunner add/list/run/history/logs` em outro, no Linux, com o
  mesmo comportamento de hoje.
- Dois clientes simultâneos recebem os eventos de uma execução disparada por um terceiro.
- Matar o daemon no meio de uma execução e subir de novo marca a execução como `interrupted`, e os clientes
  recebem erro claro enquanto ele está fora.
- Todos os testes, clippy e fmt passam no CI em ubuntu **e** windows. O teste de named pipe é `cfg(windows)` e
  roda no job do Windows.

### Checklist manual no Windows (o que só você consegue confirmar)

1. `scriptrunner daemon` sobe e cria o pipe `\\.\pipe\scriptrunner`.
2. `scriptrunner list` em outro terminal responde.
3. Uma execução `.bat` aparece em tempo real no `run`, com acentos corretos.
4. Fechar o daemon com Ctrl+C e subir de novo preserva tarefas e histórico.
5. Um segundo daemon no mesmo pipe falha com mensagem clara, em vez de duplicar o scheduler.

---

## Riscos

| Risco | Mitigação |
|---|---|
| Os testes de Windows do CI podem falhar de início (engine e executor eram `cfg(unix)`) | Tratar como achado da leva 0. Corrigir antes de começar a leva 1 |
| Reformatação da 0.2 gera um diff enorme | Commit isolado, sem mistura com lógica |
| Servidor IPC com muitos clientes bloqueando a thread do SQLite | Tudo via `blocking` (0.5) e limite de conexões na 1.5 |
| Haiku produzir código que compila mas viola a especificação | Cada entrega de Haiku passa por `cargo test` e `clippy` e pela minha leitura do diff antes de integrar |
| Segundo daemon no mesmo banco | Pipe com `first_pipe_instance`: o segundo falha ao criar |

## Achados durante a execução da leva 0

- **Bug pré-existente no scheduler (corrigido):** os testes `scheduler_fires_interval_tasks_without_duplicates` e
  `disabled_schedules_do_not_fire_but_manual_runs_work` travavam em cerca de 25% das execuções, **também no
  commit anterior à leva 0**. Causa: a âncora de um intervalo guarda nanossegundos, mas o banco trunca os
  horários para milissegundo. O slot gravado ficava um pouco antes do real, e um acordar dentro dessa fração
  de milissegundo via o slot como ainda vencido e o disparava de novo, em laço infinito, com `now` fixo.
  No daemon real isso criaria execuções sem parar. Correção: alinhar a âncora ao milissegundo em
  `next_interval` e proteger `fire_due` contra slot que não avança. Depois da correção, 0 travamentos em 40
  rodadas (antes, 5 em 20).
- **Toolchain fixada e target Windows:** com o `rust-toolchain.toml`, o `rustup target add
  x86_64-pc-windows-gnu` passa a valer para a toolchain fixada. Quem já tinha o target na toolchain `stable`
  precisa adicioná-lo de novo.

## Pesquisa concluída

- **Toolchain, `fmt` e CI (verificado):**
  - `rustc 1.99.1` local, que satisfaz o mínimo 1.88.
  - `clippy -D warnings` já passa; `fmt --check` não passa (ver 0.2).
  - Rascunho do workflow pronto, com permissões mínimas e cancelamento de execuções antigas na mesma ref.
  - O runner `windows-latest` tem MSVC, então o SQLite embutido compila. O mapeamento exato de `windows-latest`
    para a imagem não foi confirmado e será validado pela primeira execução.
  - O rascunho ainda não passou por `actionlint`.

- **IPC (tokio 1.54.1, tokio-util 0.7.20, `interprocess` 2.4.8; fontes: docs.rs):**
  - **D1 confirmada: camada própria sobre o tokio, sem `interprocess`.** Os controles de segurança de que
    precisamos (`reject_remote_clients`, `first_pipe_instance`, ACL) estão verificados na API do tokio. Na
    `interprocess` não foi possível confirmar que existam.
  - **D2 confirmada.** `LengthDelimitedCodec` já vem com prefixo u32 big-endian e limite padrão de 8 MB, o que
    coincide com a decisão. Usar `tokio::io::split` no stream e `new_read` e `new_write` do builder, para uma
    tarefa enviar eventos enquanto outra lê requisições. Isso exige `futures-util` (feature `sink`) como
    dependência do `ipc`.
  - **Windows, servidor:** laço de accept com `first_pipe_instance(true)` só na primeira instância e
    `reject_remote_clients(true)`; cria-se a próxima instância **antes** de entregar a conectada. Erro de `connect`
    é logado e o laço continua. O segundo daemon falha na criação, o que cobre o risco de dois schedulers.
  - **Windows, cliente:** `ClientOptions::open` com nova tentativa a cada 50 ms em `ERROR_PIPE_BUSY` (231).
  - **ACL só por API `unsafe`:** o tokio expõe apenas `create_with_security_attributes_raw`, que recebe um
    `SECURITY_ATTRIBUTES` cru. Não há API segura. Isso confirma que a ACL completa fica na leva 2.
  - **Unix, socket obsoleto:** um arquivo de socket sobrando faz o `bind` falhar com `AddrInUse`. Regra para a
    tarefa 1.4: tentar conectar primeiro; se conectar, já existe um daemon vivo (erro claro); se recusar, o
    arquivo é obsoleto, então remove e faz o `bind`.
  - **Unix, permissão:** `set_permissions` depois do `bind` deixa uma janela com o socket mais aberto. Em vez
    disso, criar o socket **dentro de um diretório 0700**. O acesso fica restrito desde o início, e a janela
    deixa de importar.
