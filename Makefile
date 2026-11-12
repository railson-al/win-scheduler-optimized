# ScriptRunner: atalhos para os comandos do dia a dia no Linux.
#
# `make` sem argumentos lista os alvos. O que roda no Windows (build, service install, GUI) está em
# `make windows-steps`, porque o Windows normalmente não tem `make`.

CARGO ?= cargo

# Alvo do type-check do código Windows a partir do Linux. O compilador de C "falso" só satisfaz o
# build.rs do SQLite embutido: `clippy` nunca linka, então ele nunca é chamado de verdade.
WIN_TARGET := x86_64-pc-windows-gnu
WIN_ENV    := CC_x86_64_pc_windows_gnu=true AR_x86_64_pc_windows_gnu=true

# Pasta de dados do ambiente de desenvolvimento. Precisa ser CURTA: o caminho do socket Unix
# (<dir>/ipc/...) tem limite de ~108 bytes. Sobrescreva com `make daemon DEV_DIR=/outro/caminho`.
DEV_DIR ?= $(HOME)/.cache/sr-dev
export SCRIPTRUNNER_DATA_DIR := $(DEV_DIR)

# Argumentos repassados à CLI em `make sr ARGS="..."`.
ARGS ?= list

.DEFAULT_GOAL := help
.PHONY: help setup build release test test-one fmt fmt-check clippy check-windows ci \
        daemon sr gui dev-reset touch windows-steps clean

help: ## Lista os alvos
	@awk 'BEGIN {FS = ":.*## "} /^[a-zA-Z_-]+:.*## / {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

setup: ## Instala o target do Windows para o type-check (uma vez)
	rustup target add $(WIN_TARGET)

# --- Qualidade: o mesmo que o CI roda ----------------------------------------------------------

build: ## Compila o workspace (debug)
	$(CARGO) build --workspace

release: ## Compila o workspace (release)
	$(CARGO) build --workspace --release

test: ## Roda todos os testes
	$(CARGO) test --workspace

test-one: ## Roda os testes que casam com T (make test-one T=editor)
	@test -n "$(T)" || { echo "uso: make test-one T=<filtro>"; exit 2; }
	$(CARGO) test --workspace -- $(T)

fmt: ## Formata o código
	$(CARGO) fmt --all

fmt-check: ## Confere a formatação (como o CI)
	$(CARGO) fmt --all -- --check

clippy: ## Clippy no Linux, avisos viram erro (como o CI)
	$(CARGO) clippy --workspace --all-targets -- -D warnings

check-windows: ## Clippy no código cfg(windows), sem precisar do Windows nem do mingw
	$(WIN_ENV) $(CARGO) clippy --workspace --target $(WIN_TARGET) --all-targets -- -D warnings

ci: fmt-check clippy check-windows test ## Tudo que o CI confere, mais o código do Windows

# --- Rodar localmente no Linux -----------------------------------------------------------------

daemon: ## Sobe o daemon em primeiro plano (Ctrl+C para sair), dados em DEV_DIR
	@mkdir -p $(DEV_DIR)
	$(CARGO) run -p scriptrunner-service --bin scriptrunner -- daemon

sr: ## Roda um comando da CLI contra o daemon (make sr ARGS="add Hello /bin/true --every 1m")
	$(CARGO) run -q -p scriptrunner-service --bin scriptrunner -- $(ARGS)

gui: ## Abre a GUI contra o daemon de DEV_DIR (suba o `make daemon` antes)
	$(CARGO) run -p scriptrunner-gui --bin scriptrunner-gui

dev-reset: ## Apaga os dados de desenvolvimento (DEV_DIR). Pare o daemon antes
	rm -rf $(DEV_DIR)

# --- Utilidades --------------------------------------------------------------------------------

# Arquivos restaurados de um backup (tar, cp -p) mantêm o mtime antigo e o cargo reaproveita um
# binário velho. Atualiza o mtime dos fontes para forçar a recompilação.
touch: ## Atualiza o mtime dos fontes (depois de restaurar arquivos de backup)
	find crates \( -name '*.rs' -o -name Cargo.toml -o -name build.rs \) -exec touch {} +

clean: ## Apaga target/
	$(CARGO) clean

windows-steps: ## Imprime a sequência de comandos para testar no Windows (PowerShell elevado)
	@printf '%s\n' \
	  '# No Windows, em C:\dev\projects\win-scheduler-optimized (PowerShell como administrador):' \
	  'git pull' \
	  'cargo build -p scriptrunner-service -p scriptrunner-gui' \
	  '' \
	  '# Serviço: reinstalar com o novo .exe (os dados são mantidos)' \
	  '.\target\debug\scriptrunner.exe service uninstall' \
	  '.\target\debug\scriptrunner.exe service install' \
	  '.\target\debug\scriptrunner.exe service start' \
	  '.\target\debug\scriptrunner.exe service status' \
	  '' \
	  '# GUI (pede elevação)' \
	  '.\target\debug\scriptrunner-gui.exe' \
	  '' \
	  '# Tarefa que mostra janela na sessão do usuário logado' \
	  '.\target\debug\scriptrunner.exe add Teste notepad.exe --every 1m --user-session' \
	  '.\target\debug\scriptrunner.exe run Teste'
