@echo off
rem ============================================================================
rem  ScriptRunner - instalador para Windows.
rem
rem  Compila o release, instala o servico "ScriptRunner" e o inicia. Pode ser
rem  executado de novo a qualquer momento para atualizar: o servico antigo e
rem  removido e o novo instalado, e os dados (banco e logs) sao mantidos.
rem
rem  Requisitos: Rust (rustup) e Visual Studio Build Tools com a carga
rem  "Desktop development with C++". Execute como administrador.
rem
rem  Argumentos opcionais sao repassados a "service install", por exemplo:
rem      win-installer.bat --data-dir D:\scriptrunner\dados
rem
rem  Defina NO_PAUSE=1 para nao esperar uma tecla no final (uso em automacao).
rem
rem  Este arquivo deve usar fim de linha CRLF (ver .gitattributes).
rem ============================================================================
setlocal EnableExtensions
cd /d "%~dp0"

call :run %*
set "RC=%errorlevel%"

echo.
if "%RC%"=="0" (
    echo === Concluido ===
) else (
    echo === Falhou, codigo %RC% ===
)
if not defined NO_PAUSE pause
exit /b %RC%

:run
set "SR=%~dp0target\release\scriptrunner.exe"
set "GUI=%~dp0target\release\scriptrunner-gui.exe"

echo.
echo === ScriptRunner: instalador ===

rem Administrador: instalar o servico e abrir o pipe exigem elevacao.
fltmc >nul 2>&1
if errorlevel 1 (
    echo.
    echo [ERRO] Execute este arquivo como administrador: botao direito e "Executar como administrador".
    exit /b 1
)

where cargo >nul 2>&1
if errorlevel 1 (
    echo.
    echo [ERRO] O cargo nao foi encontrado no PATH. Instale o Rust em https://rustup.rs
    echo        e abra um terminal novo. Tambem e preciso o Visual Studio Build Tools
    echo        com a carga "Desktop development with C++".
    exit /b 1
)

echo.
echo [1/4] Compilando o release. A primeira vez demora alguns minutos.
cargo build --release -p scriptrunner-service -p scriptrunner-gui
if errorlevel 1 (
    echo.
    echo [ERRO] A compilacao falhou. Veja a mensagem acima.
    exit /b 1
)

echo.
echo [2/4] Removendo uma instalacao anterior, se houver. Os dados sao mantidos.
rem Falha quando o servico ainda nao existe, e isso e esperado: a saida e descartada.
"%SR%" service uninstall >nul 2>&1

echo.
echo [3/4] Instalando o servico.
"%SR%" service install %*
if errorlevel 1 (
    echo.
    echo [ERRO] Nao foi possivel instalar o servico.
    exit /b 1
)

echo.
echo [4/4] Iniciando o servico.
"%SR%" service start
if errorlevel 1 (
    echo.
    echo [ERRO] Nao foi possivel iniciar o servico.
    exit /b 1
)
"%SR%" service status

echo.
echo Pronto. O servico ScriptRunner esta rodando e sobe junto com o Windows.
echo.
echo   Interface grafica ^(abre pedindo elevacao^):
echo     "%GUI%"
echo.
echo   Linha de comando ^(terminal como administrador^):
echo     "%SR%" list
echo.
echo   Para atualizar depois de um "git pull", execute este arquivo de novo.
exit /b 0
