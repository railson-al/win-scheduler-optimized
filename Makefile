# ScriptRunner: atalhos para os comandos do dia a dia, no Linux e no Windows.
#
# `make` sem argumentos lista os alvos. No Windows o make roda as receitas pelo cmd.exe (não exige sh,
# awk nem Git Bash), então aqui só entram comandos que existem nos dois lados ou que são separados por
# plataforma. Os alvos de serviço exigem um terminal elevado (Executar como administrador).

CARGO ?= cargo
ARGS  ?= list

ifeq ($(OS),Windows_NT)
  WINDOWS := 1
  SHELL := cmd.exe
  .SHELLFLAGS := /c
  # Mesma pasta de dados dos exemplos do README para o daemon em modo console.
  DEV_DIR ?= C:\sr-teste\data
  SR_EXE  := target\debug\scriptrunner.exe
  RM_DEV   = if exist "$(DEV_DIR)" rmdir /s /q "$(DEV_DIR)"
  NOP      = rem
  .PHONY: service-install service-uninstall service-start service-stop service-status service-reinstall
else
  WINDOWS :=
  # Precisa ser CURTA: o caminho do socket Unix (<dir>/ipc/...) tem limite de ~108 bytes.
  DEV_DIR ?= $(HOME)/.cache/sr-dev
  RM_DEV   = rm -rf "$(DEV_DIR)"
  NOP      = :
  .PHONY: setup check-windows touch
  # Type-check do código Windows a partir do Linux. O compilador de C "falso" só satisfaz o build.rs do
  # SQLite embutido: `clippy` nunca linka, então ele nunca é chamado de verdade.
  WIN_TARGET := x86_64-pc-windows-gnu
  WIN_ENV    := CC_x86_64_pc_windows_gnu=true AR_x86_64_pc_windows_gnu=true
endif

export SCRIPTRUNNER_DATA_DIR := $(DEV_DIR)

.DEFAULT_GOAL := help
.PHONY: help build release test test-one fmt fmt-check clippy ci daemon sr gui dev-reset clean

# A ajuda é feita só com funções do make ($(info)), para não depender de awk nem de sh.
define HELP_COMMON

Alvos (qualquer sistema):
  build              Compila o workspace (debug)
  release            Compila o workspace (release)
  test               Roda todos os testes
  test-one T=filtro  Roda só os testes que casam com o filtro (make test-one T=editor)
  fmt                Formata o código
  fmt-check          Confere a formatação (como o CI)
  clippy             Clippy com avisos como erro (como o CI)
  ci                 O que o CI confere (formatação, clippy, testes)
  daemon             Daemon em primeiro plano (Ctrl+C sai), dados em DEV_DIR
  sr ARGS="..."      Comando da CLI contra o daemon (make sr ARGS="add Hello x.bat --every 1m")
  gui                Abre a GUI (suba o daemon ou o serviço antes)
  dev-reset          Apaga a pasta de dados de desenvolvimento (DEV_DIR); pare o daemon antes
  clean              Apaga target/
endef

define HELP_LINUX

Só no Linux:
  setup              Instala o target do Windows para o type-check (uma vez)
  check-windows      Clippy no código cfg(windows), sem Windows nem mingw (também roda no ci)
  touch              Atualiza o mtime dos fontes depois de restaurar arquivos de backup
endef

define HELP_WINDOWS

Só no Windows (terminal elevado):
  service-reinstall  Compila, remove e instala de novo o serviço com o novo .exe, e inicia
  service-install    Instala o serviço (cópia do .exe vai para Program Files)
  service-uninstall  Para e remove o serviço (mantém os dados)
  service-start      Inicia o serviço
  service-stop       Para o serviço
  service-status     Mostra o estado do serviço
endef

help:
	$(info $(HELP_COMMON))
	$(info $(if $(WINDOWS),$(HELP_WINDOWS),$(HELP_LINUX)))
	$(info )
	$(info DEV_DIR = $(DEV_DIR))
	@$(NOP)

# --- Qualidade: o mesmo que o CI roda ----------------------------------------------------------

build:
	$(CARGO) build --workspace

release:
	$(CARGO) build --workspace --release

test:
	$(CARGO) test --workspace

test-one:
	$(if $(T),,$(error uso: make test-one T=<filtro>))
	$(CARGO) test --workspace -- $(T)

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

ifeq ($(WINDOWS),1)
# No Windows o clippy acima já compila o código cfg(windows).
ci: fmt-check clippy test
else
setup:
	rustup target add $(WIN_TARGET)

check-windows:
	$(WIN_ENV) $(CARGO) clippy --workspace --target $(WIN_TARGET) --all-targets -- -D warnings

ci: fmt-check clippy check-windows test

# Arquivos restaurados de um backup (tar, cp -p) mantêm o mtime antigo e o cargo reaproveita um
# binário velho. Atualiza o mtime dos fontes para forçar a recompilação.
touch:
	find crates \( -name '*.rs' -o -name Cargo.toml -o -name build.rs \) -exec touch {} +
endif

# --- Rodar localmente ----------------------------------------------------------------------------

# O daemon cria a pasta de dados sozinho. No Windows ele exige terminal elevado e que o serviço
# NÃO esteja instalado (o modo console não se mistura com o serviço).
daemon:
	$(CARGO) run -p scriptrunner-service --bin scriptrunner -- daemon

sr:
	$(CARGO) run -q -p scriptrunner-service --bin scriptrunner -- $(ARGS)

gui:
	$(CARGO) run -p scriptrunner-gui --bin scriptrunner-gui

dev-reset:
	$(RM_DEV)

clean:
	$(CARGO) clean

# --- Serviço do Windows (terminal elevado) -----------------------------------------------------------

ifeq ($(WINDOWS),1)
service-install:
	$(SR_EXE) service install

service-uninstall:
	$(SR_EXE) service uninstall

service-start:
	$(SR_EXE) service start

service-stop:
	$(SR_EXE) service stop

service-status:
	$(SR_EXE) service status

# O "-" na frente do uninstall ignora o erro de quando o serviço ainda não existe. Os dados são mantidos.
service-reinstall:
	$(CARGO) build -p scriptrunner-service -p scriptrunner-gui
	-$(SR_EXE) service uninstall
	$(SR_EXE) service install
	$(SR_EXE) service start
	$(SR_EXE) service status
endif
