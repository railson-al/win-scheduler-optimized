//! Texts of the task editor (`views/editor.rs`). Add a `Text` variant and its pt-BR line for every
//! string the screen shows; common words (Save, Cancel...) are in `super::Text`.

use super::Lang;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Text {
    // Titles and states of the screen.
    NewTitle,
    EditTitle,
    Loading,
    NotFound,
    NotFoundHint,
    Back,
    Saving,
    SaveFailed,
    PickerOpen,
    FixToSave,
    NotConnectedHint,

    // Sections.
    SectionGeneral,
    SectionScript,
    SectionScriptCaption,
    SectionExecution,
    SectionExecutionCaption,
    SectionSchedule,
    SectionScheduleCaption,
    SectionPreview,

    // General fields.
    Name,
    HelpName,
    Description,
    HelpDescription,
    Script,
    HelpScript,
    ChooseFile,
    ChooseFolder,
    DetectedType,
    Arguments,
    HelpArguments,
    Interpreter,
    HelpInterpreter,
    WorkingDir,
    HelpWorkingDir,
    Timeout,
    HelpTimeout,
    UnitSeconds,
    UnitMinutes,
    UnitHours,
    AllowConcurrent,
    HelpAllowConcurrent,
    CaptureLogs,
    HelpCaptureLogs,
    RunInUserSession,
    HelpRunInUserSession,
    ScheduleEnabled,
    HelpScheduleEnabled,

    // Schedule.
    ScheduleType,
    KindInterval,
    KindDaily,
    KindWeekly,
    KindCron,
    HelpInterval,
    Time,
    HelpTime,
    Days,
    HelpDays,
    CronExpr,
    HelpCron,
    PreviewNone,
    PreviewInvalid,

    // Script types, as the editor names them.
    TypePython,
    TypeBat,
    TypeCmd,
    TypePowerShell,
    TypeExe,

    // File dialogs.
    ChooseScriptTitle,
    ChooseInterpreterTitle,
    ChooseFolderTitle,
    FilterScripts,
    FilterExecutables,

    // Leaving with unsaved changes.
    DiscardTitle,
    DiscardBody,
    Discard,
    KeepEditing,

    // Problems with the form (see `views::editor::problem_text`).
    ProblemRequiredField,
    ProblemRequiredName,
    ProblemRequiredScript,
    ProblemUnknownScript,
    ProblemNotNumber,
    ProblemNotPositive,
    ProblemOutOfRange,
    ProblemBadTime,
    ProblemNoDays,
    ProblemBadCron,
}

pub fn text(lang: Lang, key: Text) -> &'static str {
    match lang {
        Lang::PtBr => pt_br(key),
    }
}

/// [`text`] in the default language.
pub fn t(key: Text) -> &'static str {
    text(Lang::default(), key)
}

fn pt_br(key: Text) -> &'static str {
    match key {
        Text::NewTitle => "Nova tarefa",
        Text::EditTitle => "Editar tarefa",
        Text::Loading => "Carregando…",
        Text::NotFound => "Tarefa não encontrada",
        Text::NotFoundHint => "Ela pode ter sido excluída. Volte à lista de tarefas.",
        Text::Back => "Voltar",
        Text::Saving => "Salvando…",
        Text::SaveFailed => "Não foi possível salvar a tarefa.",
        Text::PickerOpen => "Aguardando a janela de seleção de arquivo…",
        Text::FixToSave => "Preencha os campos marcados para poder salvar.",
        Text::NotConnectedHint => "Sem conexão com o serviço. Não é possível salvar agora.",

        Text::SectionGeneral => "Geral",
        Text::SectionScript => "Script",
        Text::SectionScriptCaption => "O que roda, com quais argumentos e em qual pasta.",
        Text::SectionExecution => "Execução",
        Text::SectionExecutionCaption => "Limites e o que acontece a cada execução.",
        Text::SectionSchedule => "Agenda",
        Text::SectionScheduleCaption => "Quando a tarefa roda sozinha.",
        Text::SectionPreview => "Próximas execuções",

        Text::Name => "Nome",
        Text::HelpName => "Como a tarefa aparece na lista.",
        Text::Description => "Descrição",
        Text::HelpDescription => "Opcional. Uma explicação curta do que ela faz.",
        Text::Script => "Script",
        Text::HelpScript => "Arquivo .py, .pyw, .bat, .cmd, .ps1 ou .exe.",
        Text::ChooseFile => "Escolher…",
        Text::ChooseFolder => "Escolher pasta…",
        Text::DetectedType => "Tipo detectado:",
        Text::Arguments => "Argumentos",
        Text::HelpArguments => "Um argumento por linha. Espaços dentro de um argumento não precisam de aspas.",
        Text::Interpreter => "Interpretador",
        Text::HelpInterpreter => "Opcional, por exemplo o python de um venv.",
        Text::WorkingDir => "Pasta de trabalho",
        Text::HelpWorkingDir => "Padrão: a pasta do script.",
        Text::Timeout => "Tempo limite",
        Text::HelpTimeout => "Vazio = sem limite de tempo.",
        Text::UnitSeconds => "segundos",
        Text::UnitMinutes => "minutos",
        Text::UnitHours => "horas",
        Text::AllowConcurrent => "Permitir execuções simultâneas",
        Text::HelpAllowConcurrent => "Desmarcado: a tarefa não roda duas vezes ao mesmo tempo.",
        Text::CaptureLogs => "Guardar logs",
        Text::HelpCaptureLogs => "Guarda a saída de cada execução para consulta no histórico.",
        Text::RunInUserSession => "Executar na sessão do usuário logado (janela visível)",
        Text::HelpRunInUserSession => {
            "Marque para scripts que abrem janelas (um navegador com headless desligado, por exemplo) ou que \
             precisam do perfil do usuário: OneDrive, unidades de rede, logins salvos. Exige alguém logado \
             quando a tarefa disparar; sem isso a execução falha. Desmarcado, roda em segundo plano como o \
             serviço, mesmo sem ninguém logado, mas sem janela visível."
        }
        Text::ScheduleEnabled => "Agenda ativa",
        Text::HelpScheduleEnabled => "Desmarcada, a agenda fica pausada e a tarefa roda só manualmente.",

        Text::ScheduleType => "Tipo",
        Text::KindInterval => "Intervalo",
        Text::KindDaily => "Diária",
        Text::KindWeekly => "Semanal",
        Text::KindCron => "Cron",
        Text::HelpInterval => "Quantidade e unidade do intervalo.",
        Text::Time => "Horário",
        Text::HelpTime => "Use HH:MM, por exemplo 09:30.",
        Text::Days => "Dias",
        Text::HelpDays => "Os dias da semana em que a tarefa roda.",
        Text::CronExpr => "Expressão cron",
        Text::HelpCron => {
            "Cinco campos: minuto, hora, dia do mês, mês e dia da semana. Exemplo: 0 9 * * 1-5 (de segunda a sexta, às 09:00)."
        }
        Text::PreviewNone => "Sem agenda: só executa manualmente.",
        Text::PreviewInvalid => "A prévia aparece quando a agenda estiver válida.",

        Text::TypePython => "Python",
        Text::TypeBat => "Lote (.bat)",
        Text::TypeCmd => "Comando (.cmd)",
        Text::TypePowerShell => "PowerShell (.ps1)",
        Text::TypeExe => "Executável (.exe)",

        Text::ChooseScriptTitle => "Escolha o script",
        Text::ChooseInterpreterTitle => "Escolha o interpretador",
        Text::ChooseFolderTitle => "Escolha a pasta de trabalho",
        Text::FilterScripts => "Scripts",
        Text::FilterExecutables => "Executáveis",

        Text::DiscardTitle => "Descartar alterações?",
        Text::DiscardBody => "As alterações feitas nesta tarefa ainda não foram salvas.",
        Text::Discard => "Descartar",
        Text::KeepEditing => "Continuar editando",

        Text::ProblemRequiredField => "Este campo é obrigatório.",
        Text::ProblemRequiredName => "Informe o nome da tarefa.",
        Text::ProblemRequiredScript => "Informe o caminho do script.",
        Text::ProblemUnknownScript => "O tipo do script não é reconhecido (use .py, .bat, .cmd, .ps1 ou .exe).",
        Text::ProblemNotNumber => "Digite um número inteiro.",
        Text::ProblemNotPositive => "Deve ser maior que zero.",
        Text::ProblemOutOfRange => "Valor fora do permitido.",
        Text::ProblemBadTime => "Horário inválido (use HH:MM).",
        Text::ProblemNoDays => "Escolha pelo menos um dia.",
        Text::ProblemBadCron => "Expressão cron inválida.",
    }
}
