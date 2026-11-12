//! Texts of the settings screen (`views/settings.rs`). Add a `Text` variant and its pt-BR line for every
//! string the screen shows; common words (Save, Cancel...) are in `super::Text`.

use super::Lang;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Text {
    Title,
    Intro,
    Loading,
    ConnectToSave,
    ServiceChanged,
    Discard,

    SectionHistory,
    RetentionLabel,
    RetentionHelp,

    SectionLogs,
    LogPerExecutionLabel,
    LogPerExecutionHelp,
    LogTotalLabel,
    LogTotalHelp,

    SectionRuns,
    ConcurrentLabel,
    ConcurrentHelp,
    TimeoutCheck,
    TimeoutLabel,
    TimeoutHelp,
    ScriptsDirLabel,
    ScriptsDirHelp,
    PythonLabel,
    PythonHelp,

    UnitMib,
    UnitSeconds,

    ErrConcurrency,
    ErrLogLimit,
    ErrLogPerAboveTotal,
    ErrTimeout,
    ErrNotNumber,
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
        Text::Title => "Configurações",
        Text::Intro => "As alterações valem na hora, sem reiniciar o serviço.",
        Text::Loading => "Carregando…",
        Text::ConnectToSave => "Conecte-se ao serviço para salvar.",
        Text::ServiceChanged => "As configurações do serviço mudaram. Suas alterações não salvas foram mantidas.",
        Text::Discard => "Descartar alterações",

        Text::SectionHistory => "Histórico",
        Text::RetentionLabel => "Guardar histórico por (dias)",
        Text::RetentionHelp => "Dias de histórico que ficam guardados. 0 guarda todo o histórico.",

        Text::SectionLogs => "Logs",
        Text::LogPerExecutionLabel => "Tamanho máximo do log de uma execução",
        Text::LogPerExecutionHelp => "Vale para o log de cada execução separadamente.",
        Text::LogTotalLabel => "Tamanho máximo de todos os logs",
        Text::LogTotalHelp => "Soma de todos os logs guardados. Não pode ser menor que o limite de uma execução.",

        Text::SectionRuns => "Execuções",
        Text::ConcurrentLabel => "Execuções ao mesmo tempo",
        Text::ConcurrentHelp => "Quantas tarefas podem rodar juntas. Vale na hora para as próximas execuções.",
        Text::TimeoutCheck => "Usar tempo limite padrão",
        Text::TimeoutLabel => "Tempo limite padrão",
        Text::TimeoutHelp => "Encerra a execução que passar deste tempo, quando a tarefa não define o seu próprio.",
        Text::ScriptsDirLabel => "Pasta padrão dos scripts",
        Text::ScriptsDirHelp => "Pasta sugerida para os scripts. Vazio: nenhuma.",
        Text::PythonLabel => "Python padrão",
        Text::PythonHelp => "Executável do Python para os scripts .py. Vazio: nenhum.",

        Text::UnitMib => " MiB",
        Text::UnitSeconds => " s",

        Text::ErrConcurrency => "Use pelo menos 1 execução ao mesmo tempo.",
        Text::ErrLogLimit => "O limite precisa ser maior que zero.",
        Text::ErrLogPerAboveTotal => "O limite de uma execução não pode ser maior que o total.",
        Text::ErrTimeout => "O tempo limite precisa ser de pelo menos 1 segundo.",
        Text::ErrNotNumber => "Valor inválido.",
    }
}
