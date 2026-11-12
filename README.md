# ScriptRunner

Agendador de scripts para Windows escrito em Rust: cadastra, agenda, executa e monitora
scripts (`.py`, `.bat`, `.cmd`, `.ps1`, `.exe`) com histórico e logs, sem depender do
Task Scheduler.

**Estado atual:** motor (fase 1), IPC local (leva 1) e serviço do Windows (leva 2) prontos. O `daemon`
roda em modo console, e o serviço `ScriptRunner` roda o mesmo daemon como serviço do Windows. A CLI
`scriptrunner` fala com o daemon por IPC. A GUI (egui) ainda não existe.

O desenvolvedor trabalha no Linux, então as partes específicas do Windows (pipe, serviço, ACLs) não
puderam ser executadas localmente. Elas são checadas pelo CI do Windows e pelo checklist manual abaixo.

## Estrutura

| Crate | Papel |
|---|---|
| `crates/core` | Modelos, cálculo de agendamento, executor de processos, logs |
| `crates/storage` | SQLite: tarefas, histórico, retenção, configurações |
| `crates/ipc` | Protocolo (frames JSON), transporte (named pipe com ACL restrita a SYSTEM e Administradores; socket Unix no Linux) e cliente; não depende de SQLite |
| `crates/winsvc` | Plumbing do serviço Windows: dispatcher do SCM, instalação e remoção, permissões da pasta de dados. Vazio fora do Windows; não depende de SQLite |
| `crates/service` | Motor (agendador + execuções), servidor IPC e a CLI `scriptrunner` |

## Compilar e testar no Windows

Requer [Rust](https://rustup.rs) e o *Visual Studio Build Tools* com a carga
**Desktop development with C++** (compila o SQLite embutido).

```powershell
cargo test --workspace
cargo build --release
```

O executável fica em `target\release\scriptrunner.exe`.

## Usar a CLI

O `daemon` é o dono do banco e do agendador. Todos os outros comandos são clientes dele por IPC
(named pipe `\\.\pipe\scriptrunner` no Windows; socket em `<data_dir>/ipc/` no Linux). Sem o daemon
rodando, eles falham com uma mensagem clara. Use dois terminais:

```powershell
# Terminal 1: o daemon
$env:SCRIPTRUNNER_DATA_DIR = "C:\sr-teste\data"   # onde ficam o banco e os logs
.\target\release\scriptrunner.exe daemon          # Ctrl+C para sair
```

```powershell
# Terminal 2: os comandos
$env:SCRIPTRUNNER_DATA_DIR = "C:\sr-teste\data"
$sr = ".\target\release\scriptrunner.exe"

& $sr add "Backup" "C:\scripts\backup.py" --every 15m
& $sr add "Relatorio" "C:\scripts\rel.bat" --daily 08:30
& $sr add "Semanal" "C:\scripts\x.ps1" --weekly "mon,fri 09:30"
& $sr add "Cron" "C:\scripts\y.py" --cron "*/5 * * * 1-5" --timeout 60

& $sr list
& $sr run Backup          # executa agora, mostrando a saída
& $sr history             # histórico
& $sr logs <id-da-execucao>   # aceita o id completo ou um prefixo único
```

Outros comandos: `enable`, `disable`, `delete`.

## Instalar como serviço do Windows

Com o serviço instalado, o motor roda sem nenhuma sessão aberta e sobe com a máquina. Os comandos
abaixo exigem um terminal **elevado** (Executar como administrador):

```powershell
.\target\release\scriptrunner.exe service install     # cria o serviço, sem iniciar
.\target\release\scriptrunner.exe service start
.\target\release\scriptrunner.exe service status
.\target\release\scriptrunner.exe service stop
.\target\release\scriptrunner.exe service uninstall   # para, remove e mantém os dados
```

Para usar outra pasta de dados, passe `--data-dir` (ou `SCRIPTRUNNER_DATA_DIR`) junto de `service install`.
O caminho fica gravado no serviço.

- **Conta: LocalSystem.** Os scripts rodam como SYSTEM, na sessão 0: nenhuma janela aparece para o
  usuário, não há unidades de rede mapeadas nem perfil do usuário (HKCU). O acesso à rede usa a conta
  da máquina. Executar como usuário fica para uma etapa futura.
- **Quem fala com o pipe: só SYSTEM e Administradores.** Por isso `list`, `run` e os demais comandos
  precisam de terminal elevado. Sem elevação, a CLI pede para executar como Administrador.
- **Dados e logs.** A pasta padrão é `%ProgramData%\ScriptRunner`. Ela fica restrita a SYSTEM e
  Administradores na instalação, e essa permissão é reaplicada a cada partida do serviço. O banco é
  `<data_dir>\scriptrunner.db` e os logs de execução ficam em `<data_dir>\logs`. Os logs do serviço ficam em
  `<data_dir>\service\scriptrunner-service.<data>.log` (um arquivo por dia, 14 mantidos). Falhas antes
  de o log existir vão para `<data_dir>\startup-error.log`.
- **Falhas e reinício.** Se o serviço cair, o SCM o reinicia após 5 s, 30 s e 60 s. A contagem volta a
  zero depois de um dia sem falhas. O início é automático (sem atraso, para ocupar o nome do pipe antes
  de qualquer outro processo), e ao desligar a máquina o serviço tem 30 s de pré-desligamento para
  parar os scripts.
- **Onde fica o executável.** `service install` copia o `.exe` para `%ProgramFiles%\ScriptRunner` e registra
  essa cópia, porque o serviço roda como SYSTEM e uma pasta gravável por usuário comum (Downloads,
  `target\`) permitiria trocar o programa. `service uninstall` remove a cópia.
- **Pasta de dados pré-existente.** Qualquer usuário pode criar pastas em `%ProgramData%`. Por isso o
  `install` e cada partida do serviço **recusam** uma pasta de dados que já exista e não pertença a SYSTEM
  ou Administradores (ou que seja um link, ou contenha arquivos de outro dono), em vez de tentar "consertar".
  Se isso acontecer, apague ou mova a pasta e instale de novo.
- **Atualizar o executável.** Rode `service uninstall` e depois `service install` com o novo `.exe`. Os
  dados são mantidos.
- **`scriptrunner daemon` no Windows** também exige um terminal elevado: ele aplica as mesmas permissões
  à pasta de dados e usa o mesmo pipe restrito a Administradores.
- **Modo console não se mistura com o serviço.** `scriptrunner daemon` é para máquinas em que o serviço
  **não** está instalado. Quando ele está instalado, a CLI só conversa com o processo que é o próprio
  serviço. Se o serviço estiver parado, a CLI diz que o daemon não está rodando, em vez de conectar a
  outro processo.

## O que conferir no Windows

- `.bat` em caminho com espaços; acentos na saída (cp850).
- `-ExecutionPolicy Bypass` em `.ps1`.
- Timeout/stop encerra o script **e os filhos** (Job Object); nenhuma janela de console aparece.
- `daemon` dispara 1 execução por slot, sem duplicatas; slots perdidos viram `skipped (missed…)`.

Checklist manual do serviço (só dá para confirmar no Windows):

1. `scriptrunner service install` num terminal elevado cria o serviço `ScriptRunner`: LocalSystem, início
   automático, com descrição, apontando para a cópia em `%ProgramFiles%\ScriptRunner`. Sem elevação, falha
   logo no começo e não deixa nada para trás.
2. `scriptrunner service start` leva o serviço a *Running* em menos de 30 s, e `scriptrunner list` (elevado)
   responde.
3. `scriptrunner list` num terminal **sem elevação** falha com mensagem pedindo Administrador, e não com um
   erro de sistema solto.
4. `taskkill /F` no processo do serviço: o SCM o reinicia, e a execução que estava rodando fica `interrupted`.
5. `scriptrunner service stop` encerra os scripts em andamento e o serviço chega a *Stopped* sem estourar o
   `wait_hint`.
6. Reiniciar a máquina: o serviço sobe sozinho, e tarefas agendadas disparam sem nenhum usuário logado.
   Slots perdidos viram `skipped (missed…)`.
7. Um usuário comum não lê `%PROGRAMDATA%\ScriptRunner` (sem acesso ao banco nem aos logs).
8. Um `.bat` com acentos na saída aparece certo no log, e nenhuma janela surge na sessão do usuário.
9. Com o serviço parado, `list` diz que o serviço não está rodando. Com um processo qualquer ocupando o nome
   do pipe, a CLI recusa conectar.
10. `scriptrunner service uninstall` remove o serviço e mantém a pasta de dados.

## Integração contínua e versão do Rust

- O CI (GitHub Actions) roda `cargo fmt --check`, `cargo clippy -D warnings` e `cargo test` em Ubuntu e Windows.
- Um job `windows service` no Windows instala o serviço, inicia, roda `list` e um `.bat` pelo IPC, confere as
  permissões da pasta de dados, para e remove o serviço (com limpeza em `always()`).
- O runner é administrador, então esse job não detecta erro de permissão para usuário comum. Por isso existe o
  item 3 do checklist manual (terminal sem elevação).
- A toolchain é fixada em `rust-toolchain.toml` (Rust 1.99.1); o mínimo suportado é 1.88, por causa da edition 2024 e de let-chains.
- Os planos de desenvolvimento estão em `docs/PLANO-LEVA-0-1.md` (levas 0 e 1) `docs/PLANO-LEVA-2.md` (serviço do Windows) e `docs/PLANO-LEVA-3.md` (interface gráfica, ainda por fazer).

## Desenvolvimento no Linux

Tudo que é portátil compila e testa no Linux (`cargo test --workspace`). O código específico
de Windows fica atrás de `cfg(windows)`; para checá-lo:

```bash
rustup target add x86_64-pc-windows-gnu
cargo check -p scriptrunner-core --target x86_64-pc-windows-gnu --all-targets
```

O `Makefile` reúne esses comandos e funciona no Linux e no Windows (no Windows as receitas rodam pelo `cmd.exe`,
sem precisar de `sh` nem `awk`). `make` lista os alvos; os principais são `make ci` (formatação, clippy e testes,
como o CI; no Linux inclui também o clippy do código Windows), `make daemon` e `make sr ARGS="list"` (daemon e CLI
com uma pasta de dados de desenvolvimento) e `make gui`. No Windows, em terminal elevado, `make service-reinstall`
compila, reinstala o serviço com o novo `.exe` e o inicia.
