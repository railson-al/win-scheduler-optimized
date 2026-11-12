//! The single table of user-visible text.
//!
//! Every string the screens show comes from [`text`], keyed by the [`Text`] enum. Adding a language
//! means adding a [`Lang`] variant and one `match` over `Text`; the compiler then lists every key the
//! new table is missing, so nothing is silently left in the old language.

use chrono::Weekday;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    PtBr,
}

/// Every fixed piece of text. Text with parts that vary (a name, a number) is composed by the caller,
/// from these pieces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Text {
    AppTitle,

    // Connection (status bar).
    Connecting,
    Connected,
    DaemonDown,
    RetryingIn,
    ReasonNotRunning,
    ReasonAccessDenied,
    ReasonProtocol,
    ReasonLost,
    ReasonOther,
    DaemonVersion,
    Uptime,
    NotConnected,

    // Execution status.
    StatusQueued,
    StatusRunning,
    StatusSuccess,
    StatusFailed,
    StatusInterrupted,
    StatusSkipped,

    // Triggers.
    TriggerManual,
    TriggerScheduled,

    // Actions.
    RunNow,
    Stop,
    Pause,
    Resume,
    Edit,
    Duplicate,
    Delete,
    Save,
    Cancel,
    Search,
    Refresh,
    Copy,
    Export,

    // Dashboard columns and empty values.
    ColName,
    ColSchedule,
    ColNextRun,
    ColLastResult,
    NoSchedule,
    SchedulePaused,
    Never,

    // Schedule summaries.
    ScheduleEvery,
    ScheduleDaily,
    ScheduleAt,
    ScheduleCron,
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

/// Short weekday name, as it appears in a schedule summary.
pub fn weekday_short(lang: Lang, day: Weekday) -> &'static str {
    match lang {
        Lang::PtBr => match day {
            Weekday::Mon => "seg",
            Weekday::Tue => "ter",
            Weekday::Wed => "qua",
            Weekday::Thu => "qui",
            Weekday::Fri => "sex",
            Weekday::Sat => "sáb",
            Weekday::Sun => "dom",
        },
    }
}

fn pt_br(key: Text) -> &'static str {
    match key {
        Text::AppTitle => "ScriptRunner",

        Text::Connecting => "Conectando ao serviço…",
        Text::Connected => "Conectado",
        Text::DaemonDown => "Serviço parado",
        Text::RetryingIn => "nova tentativa em",
        Text::ReasonNotRunning => "O serviço ScriptRunner não está em execução.",
        Text::ReasonAccessDenied => "Acesso negado ao serviço. Abra o ScriptRunner como Administrador.",
        Text::ReasonProtocol => "A interface e o serviço não conseguem se entender (versões incompatíveis?).",
        Text::ReasonLost => "A conexão com o serviço foi perdida.",
        Text::ReasonOther => "Não foi possível falar com o serviço.",
        Text::DaemonVersion => "Serviço",
        Text::Uptime => "ativo há",
        Text::NotConnected => "Sem conexão com o serviço.",

        Text::StatusQueued => "Na fila",
        Text::StatusRunning => "Executando",
        Text::StatusSuccess => "Sucesso",
        Text::StatusFailed => "Falhou",
        Text::StatusInterrupted => "Interrompida",
        Text::StatusSkipped => "Ignorada",

        Text::TriggerManual => "Manual",
        Text::TriggerScheduled => "Agendada",

        Text::RunNow => "Executar agora",
        Text::Stop => "Parar",
        Text::Pause => "Pausar agenda",
        Text::Resume => "Retomar agenda",
        Text::Edit => "Editar",
        Text::Duplicate => "Duplicar",
        Text::Delete => "Excluir",
        Text::Save => "Salvar",
        Text::Cancel => "Cancelar",
        Text::Search => "Buscar",
        Text::Refresh => "Atualizar",
        Text::Copy => "Copiar",
        Text::Export => "Exportar",

        Text::ColName => "Tarefa",
        Text::ColSchedule => "Agenda",
        Text::ColNextRun => "Próxima execução",
        Text::ColLastResult => "Último resultado",
        Text::NoSchedule => "Somente manual",
        Text::SchedulePaused => "pausada",
        Text::Never => "nunca",

        Text::ScheduleEvery => "A cada",
        Text::ScheduleDaily => "Todo dia",
        Text::ScheduleAt => "às",
        Text::ScheduleCron => "Cron",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekdays_have_names() {
        assert_eq!(weekday_short(Lang::PtBr, Weekday::Mon), "seg");
        assert_eq!(weekday_short(Lang::PtBr, Weekday::Sat), "sáb");
    }

    #[test]
    fn default_language_is_portuguese() {
        assert_eq!(t(Text::Stop), "Parar");
    }
}
