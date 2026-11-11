# ScriptRunner

Agendador de scripts para Windows escrito em Rust: cadastra, agenda, executa e monitora
scripts (`.py`, `.bat`, `.cmd`, `.ps1`, `.exe`) com histórico e logs, sem depender do
Task Scheduler.

**Estado atual:** Fase 1 (motor) pronta. Serviço Windows, IPC e GUI (egui) ainda não existem;
por enquanto há uma CLI de desenvolvimento (`scriptrunner`).

## Estrutura

| Crate | Papel |
|---|---|
| `crates/core` | Modelos, cálculo de agendamento, executor de processos, logs |
| `crates/storage` | SQLite: tarefas, histórico, retenção, configurações |
| `crates/service` | Motor (agendador + execuções) e a CLI `scriptrunner` |

## Compilar e testar no Windows

Requer [Rust](https://rustup.rs) e o *Visual Studio Build Tools* com a carga
**Desktop development with C++** (compila o SQLite embutido).

```powershell
cargo test --workspace
cargo build --release
```

O executável fica em `target\release\scriptrunner.exe`.

## Usar a CLI

```powershell
$env:SCRIPTRUNNER_DATA_DIR = "C:\sr-teste\data"   # onde ficam o banco e os logs
$sr = ".\target\release\scriptrunner.exe"

& $sr add "Backup" "C:\scripts\backup.py" --every 15m
& $sr add "Relatorio" "C:\scripts\rel.bat" --daily 08:30
& $sr add "Semanal" "C:\scripts\x.ps1" --weekly "mon,fri 09:30"
& $sr add "Cron" "C:\scripts\y.py" --cron "*/5 * * * 1-5" --timeout 60

& $sr list
& $sr run Backup          # executa agora, mostrando a saída
& $sr history             # histórico
& $sr logs <id-da-execucao>
& $sr daemon              # roda o agendador em primeiro plano (Ctrl+C para sair)
```

Outros comandos: `enable`, `disable`, `delete`.

## O que conferir no Windows

- `.bat` em caminho com espaços; acentos na saída (cp850).
- `-ExecutionPolicy Bypass` em `.ps1`.
- Timeout/stop encerra o script **e os filhos** (Job Object); nenhuma janela de console aparece.
- `daemon` dispara 1 execução por slot, sem duplicatas; slots perdidos viram `skipped (missed…)`.

## Integração contínua e versão do Rust

- O CI (GitHub Actions) roda `cargo fmt --check`, `cargo clippy -D warnings` e `cargo test` em Ubuntu e Windows.
- A toolchain é fixada em `rust-toolchain.toml` (Rust 1.99.1); o mínimo suportado é 1.88, por causa da edition 2024 e de let-chains.
- O plano de desenvolvimento das próximas etapas está em `docs/PLANO-LEVA-0-1.md`.

## Desenvolvimento no Linux

Tudo que é portátil compila e testa no Linux (`cargo test --workspace`). O código específico
de Windows fica atrás de `cfg(windows)`; para checá-lo:

```bash
rustup target add x86_64-pc-windows-gnu
cargo check -p scriptrunner-core --target x86_64-pc-windows-gnu --all-targets
```
