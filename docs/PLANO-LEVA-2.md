# Plano de desenvolvimento: Leva 2 (serviço do Windows)

Contexto: o motor e o IPC local (levas 0 e 1) estão prontos e commitados. O `daemon` roda em console e a CLI
fala com ele pelo pipe. Falta transformá-lo num serviço do Windows de verdade e fechar a segurança que a
leva 1 deixou para esta (D4 e as pendências P4 a P6 de `PLANO-LEVA-0-1.md`). A GUI (leva 3) fica para depois.

Legenda de responsável:
- **Haiku**: subtarefa mecânica ou de pesquisa, com especificação fechada. Eu reviso antes de integrar.
- **Sonnet**: desenho, concorrência, `unsafe` e integração, onde um erro sutil custa caro.

Tamanhos: S (menos de 1 hora), M (algumas horas), L (um dia ou mais).

---

## Decisões

Tomadas pelo usuário em 2026-11-11:

- **D5. Conta do serviço: LocalSystem.** Os scripts rodam como SYSTEM.
- **D6. Pipe só para SYSTEM e Administradores**, DACL `D:P(A;;GA;;;SY)(A;;GA;;;BA)`. A CLI e a GUI exigem
  elevação (UAC). Sem grupo extra e sem "usuário autenticado". Como o serviço é SYSTEM, quem alcança o pipe
  executa código como SYSTEM, então o pipe nunca pode liberar usuário comum.

Assumidas por mim (me avise se discordar):

- **D7. Pasta de dados** `%PROGRAMDATA%\ScriptRunner` com DACL protegida (sem herança) para SYSTEM e
  Administradores, aplicada na instalação e conferida a cada partida do serviço. O banco e os logs ficam ali.
- **D8. Um executável, dois modos.** `scriptrunner daemon` continua sendo o modo console (desenvolvimento).
  `scriptrunner service run` é o que o SCM executa. Se for chamado de um console, o SCM responde com o erro
  1063 e a CLI orienta a usar `daemon`. Nada de detecção automática.
- **D9. Log do serviço** em arquivo rotativo (`tracing-appender`) em `<data_dir>\service\`, diário, mantendo 14
  arquivos. Sem Event Log na v1: nenhuma camada `tracing` mantida foi encontrada para ele.
- **D10. Recuperação:** reiniciar em falha (5 s, 30 s, 60 s; zera a contagem após 1 dia), início automático
  **sem atraso** (mudou na revisão: o atraso alargava a janela em que outro processo podia ocupar o nome
  do pipe), e `Preshutdown` tratado como `Stop`. Uma falha ao parar *a pedido* sai com código 0, para o
  SCM não reiniciar um serviço que o usuário acabou de parar.
- **D12. Executável e pasta de dados (mudou na revisão de segurança):** o `install` copia o `.exe` para
  `%ProgramFiles%\ScriptRunner` e registra essa cópia, pois um `.exe` em pasta gravável por usuário comum
  seria trocado e rodaria como SYSTEM. A pasta de dados é criada já com a DACL restrita; uma pasta
  pré-existente (qualquer usuário cria pastas em `%ProgramData%`) só é aceita se o dono for SYSTEM ou
  Administradores, não for link e não tiver filhos de outro dono. Caso contrário o `install` e a partida do
  serviço recusam, sem "consertar" e sem gravar nada ali.
- **D11. Verificação do servidor pelo cliente (fecha a P4):** se o serviço `ScriptRunner` está instalado, o
  PID do servidor do pipe (`GetNamedPipeServerProcessId`) precisa ser o do processo do serviço. Instalado e
  parado: o cliente recusa conectar (um processo qualquer poderia ter tomado o nome do pipe). Não instalado:
  modo desenvolvimento, sem verificação. O cliente mantém `SECURITY_IDENTIFICATION` (padrão do tokio).

Limitações aceitas, para constar na documentação:

- Scripts rodam na sessão 0: nenhuma janela aparece para o usuário, e não há unidades de rede mapeadas nem o
  perfil (HKCU) do usuário. Acesso a rede usa a conta da máquina.
- Quem precisa de "executar como usuário" fica para uma leva futura (`CreateProcessAsUser`).
- O pesquisador de apoio recomendou uma conta virtual `NT SERVICE\ScriptRunner` por ser de menor privilégio.
  Ficou a sua escolha de LocalSystem; o pipe restrito a Administradores é o que limita o risco. Vale rever
  quando houver "executar como".

---

## Estrutura

```
crates/ipc/src/transport.rs        (Windows) ACL no pipe e verificação do servidor
crates/ipc/src/pipe_security.rs    novo, cfg(windows): SDDL -> SECURITY_ATTRIBUTES (RAII)
crates/ipc/src/service_check.rs    novo, cfg(windows): PID do serviço pelo SCM
crates/service/src/daemon.rs       novo, portátil: daemon::run(dir, shutdown, ready), compartilhado por console e serviço
crates/service/src/logging.rs      novo: log em console (dev) ou em arquivo (serviço)
crates/winsvc/                     novo crate, só Windows: lib.rs (despachante do SCM e controles), manage.rs (instalar etc.)
```

**Mudança em relação ao plano inicial:** o código do SCM ficou num crate próprio (`winsvc`), e não em
`service/src/winservice/`. Ele não depende de SQLite, então `cargo check --target x86_64-pc-windows-gnu` o
cobre no Linux sem o mingw. Para checar também o crate `service`, basta enganar o script de build do SQLite:
`CC_x86_64_pc_windows_gnu=true AR_x86_64_pc_windows_gnu=true cargo clippy --workspace --target
x86_64-pc-windows-gnu --all-targets -- -D warnings`. Isso só checa tipos e lints; nada disso executa no Linux.
Por isso a tarefa 2.0 deixou de ser necessária.

Dependências novas (Windows): `windows-service` 0.8.1 (MSRV 1.71, `windows-sys` 0.61) no `service`;
`windows-sys` 0.61.2 com `Win32_Security`, `Win32_Security_Authorization`, `Win32_System_Pipes`,
`Win32_System_Services`, `Win32_System_Threading` e `Win32_Foundation` (nomes conferidos no código do crate).
Em todas as plataformas: `tracing-appender` 0.2.5 no `service`.

## Tarefas

| # | Tarefa | Quem | Tam. | Depende de |
|---|---|---|---|---|
| 2.0 | **Opcional, do usuário:** instalar o mingw (`sudo apt install gcc-mingw-w64-x86-64`) para o `cargo check --target x86_64-pc-windows-gnu` cobrir o crate `service` inteiro. Sem ele só `core` e `ipc` são checados no Linux, e o resto só roda no CI do Windows | Usuário | S | |
| 2.1 | Extrair o corpo do `daemon` de `main.rs` para `daemon::run(dir, shutdown: CancellationToken)`: liga o endpoint antes de `startup()`, sobe agendador e servidor IPC, e ao cancelar faz o desligamento ordenado. O console cancela no Ctrl+C. Teste de integração no Linux: partida, parada por token, segundo daemon recusado | Sonnet | M | |
| 2.2 | `logging.rs`: `init_console()` e `init_file(dir)` com `tracing-appender` (diário, 14 arquivos). Um erro de partida antes de o log existir vai para um arquivo de emergência no `data_dir` | Haiku, com especificação | S | 2.1 |
| 2.3 | `pipe_security.rs` e uso em `Listener::bind` e `accept` (`create_with_security_attributes_raw`). O descritor vive tanto quanto o `Listener`, e o mesmo `SECURITY_ATTRIBUTES` vale para todas as instâncias. `LocalFree` no `Drop`. Acesso negado no cliente vira "execute como Administrador" | Sonnet | M | |
| 2.4 | Cliente: verificação do servidor (D11): PID do serviço via `QueryServiceStatusEx` contra `GetNamedPipeServerProcessId` | Sonnet | M | 2.3 |
| 2.5 | `winservice/mod.rs`: `define_windows_service!`, `service_dispatcher::start`, handler de controle (`Stop`, `Shutdown`, `Preshutdown` cancelam o token e voltam na hora), estados `StartPending` / `Running` / `StopPending` / `Stopped` com `checkpoint` e `wait_hint`, runtime tokio próprio | Sonnet | L | 2.1, 2.2 |
| 2.6 | `winservice/manage.rs` e subcomandos `service install \| uninstall \| start \| stop \| status`: cria o serviço (LocalSystem, início automático), descrição, ações de falha, `Preshutdown`, e aplica a DACL da pasta de dados (D7) antes de criar o banco | Sonnet (fluxo e DACL), Haiku (ligação da CLI) | L | 2.5 |
| 2.7 | CI: job Windows que instala, inicia, roda `list` e uma execução de `.bat`, para e remove o serviço, com limpeza em `always()` | Haiku, com especificação | S | 2.6 |
| 2.8 | README: instalação, desinstalação, elevação, sessão 0, e o checklist manual abaixo | Haiku | S | 2.6 |
| 2.9 | Revisão final com `rust-reviewer` e `security-reviewer` sobre o diff inteiro | Sonnet | S | todas |

### Paralelização

Depois da 2.1, a 2.2 (Haiku, `logging.rs`) e a 2.3 (Sonnet, `ipc`) rodam em paralelo: arquivos distintos. A 2.4 e a
2.5 também são independentes entre si (uma em `ipc`, outra em `service`). A 2.6 espera a 2.5. Nunca dois
executores no mesmo arquivo.

### Pontos que a pesquisa não confirmou (agora conferidos no código dos crates)

- Flags `ServiceControlAccept::{STOP, SHUTDOWN, PRESHUTDOWN}`, `ServiceAction { action_type, delay }`,
  `ServiceActionType::Restart` e o erro `windows_service::Error::Winapi(io::Error)` (carrega o código 1063):
  existem em `windows-service` 0.8.1. O crate também traz `set_delayed_auto_start`, `set_description` e
  `update_failure_actions`.
- Features do `windows-sys` 0.61.2 e `SDDL_REVISION_1 = 1`: conferidos.
- A DACL da pasta de dados é aplicada com `SetNamedSecurityInfoW` (SDDL `D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)`),
  sem `icacls`; a dúvida da ortografia do `icacls` deixou de existir. O verificador do servidor usa o PID do
  serviço no SCM e não precisou de `QueryFullProcessImageNameW`.

---

## Aceite da leva 2

**Automático**
- Linux: testes do `daemon::run` e todo o conjunto anterior, com clippy e fmt limpos.
- CI no Windows (ubuntu e windows): o fluxo instalar, iniciar, `list`, executar um `.bat`, parar e remover passa.
  O runner é administrador, então este job não detecta erro de ACL sozinho.

**Manual no Windows (só você consegue confirmar)**
1. `scriptrunner service install` num terminal elevado cria o serviço `ScriptRunner`: LocalSystem, início
   automático, com descrição.
2. `scriptrunner service start` leva o serviço a *Running* em menos de 30 s, e `scriptrunner list` (elevado)
   responde.
3. `scriptrunner list` num terminal **sem elevação** falha com mensagem pedindo Administrador, e não com um erro
   de sistema solto.
4. `taskkill /F` no processo do serviço: o SCM o reinicia, e a execução que estava rodando fica `interrupted`.
5. `scriptrunner service stop` encerra os scripts em andamento e o serviço chega a *Stopped* sem estourar o
   `wait_hint`.
6. Reiniciar a máquina: o serviço sobe sozinho, e tarefas agendadas disparam sem nenhum usuário logado. Slots
   perdidos viram `skipped (missed…)`.
7. Um usuário comum não lê `%PROGRAMDATA%\ScriptRunner` (sem acesso ao banco nem aos logs).
8. Um `.bat` com acentos na saída aparece certo no log, e nenhuma janela surge na sessão do usuário.
9. Com o serviço parado, `list` diz que o serviço não está rodando. Com um processo qualquer ocupando o nome
   do pipe, a CLI recusa conectar.
10. `scriptrunner service uninstall` remove o serviço e mantém a pasta de dados.

---

## Riscos

| Risco | Mitigação |
|---|---|
| Nada disto roda no Linux: o pipe, o SCM e as ACLs só se confirmam no Windows | Lógica portátil isolada em `daemon::run` e testada aqui; CI no Windows; checklist manual acima |
| `unsafe` (descritor de segurança, `SECURITY_ATTRIBUTES`) usado de forma errada | Um módulo pequeno (`pipe_security.rs`) com RAII; revisão do `security-reviewer` na 2.9 |
| ACL errada deixa o pipe aberto, ou fecha para todos (inclusive Administradores) | Aceite 2 e 3 no Windows; o CI já exercita o caminho elevado |
| O desligamento passa do `wait_hint` e o SCM mata o serviço | O handler volta na hora, `StopPending` com `checkpoint` crescente e a espera do motor é limitada (hoje 10 s) |
| Scripts como SYSTEM com impacto maior que o esperado | Limitação documentada (D5); pipe restrito a Administradores (D6) |
| Trocar o executável com o serviço rodando | `service stop` antes de substituir, documentado no README |
| Detalhes da API do `windows-service` que a pesquisa não confirmou | Lista "pontos a confirmar" acima, conferida ao implementar |

## Estado e pendências da revisão

Implementadas as tarefas 2.1 a 2.8. O código do Windows só foi checado por tipo e lint
(`cargo clippy --target x86_64-pc-windows-gnu`, workspace inteiro); **nada dele foi executado**.

Corrigido a partir da revisão de segurança e da revisão geral: pasta de dados pré-criada por usuário comum
(H1), executável em pasta gravável (H2), nome do pipe ocupado antes do serviço (início sem atraso e
mensagem), `daemon` em console sem proteger a pasta, log criado antes de proteger a pasta, ordem do
`install` (privilégio antes de tocar o disco) com remoção do serviço se um passo falha, mapeamento de erros
do SCM, falha de partida detectada logo, log da falha registrado antes de reportar `Stopped`, `engine.shutdown`
sempre executado, teto no checkpoint, panics no log e mensagem de "daemon parado" correta no Windows.

Pendente, a decidir (nenhum bloqueia o uso):

| # | Pendência | Observação |
|---|---|---|
| Q1 | A verificação do servidor consulta o SCM *antes* de abrir o pipe; um reinício do serviço nesse intervalo dá uma recusa falsa e transitória | Fecha ao abrir, comparar o PID e reconsultar uma vez |
| Q2 | Sem o serviço instalado (modo desenvolvimento) o cliente não verifica quem responde no pipe | Aceito em D11. Vale verificar o token do servidor quando houver GUI |
| Q3 | `accept()` do pipe: se criar a instância de reposição falhar, a instância conectada fica em `next` | Tratar como erro fatal ou tentar de novo |
| Q4 | Administrador sem elevação: a primeira instância do pipe nasce, as seguintes talvez falhem por ACL | Hoje o `daemon` exige terminal elevado e avisa; falta teste em Windows real |
| Q5 | Os testes do transporte no Windows falham numa máquina de desenvolvimento com o serviço instalado (a verificação vale também para pipes de teste) | Pular a verificação para `Endpoint::custom` |
| Q6 | Actions do GitHub pinadas por tag, não por SHA; o job do CI só cobre o caminho elevado | Reforçar com um teste de usuário comum e de pipe falso quando houver runner adequado |

## Fora desta leva

GUI (leva 3), instalador MSI e benchmarks (fase 4), Event Log, "executar como usuário", e as pendências P1
(paginação de logs), P2 (compatibilidade de versões do protocolo), P3 (respostas fora de ordem) e P6 de
`PLANO-LEVA-0-1.md`, que devem ser decididas antes da GUI.
