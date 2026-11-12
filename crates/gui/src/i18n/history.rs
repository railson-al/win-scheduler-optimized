//! Texts of the history screen (`views/history.rs`). Add a `Text` variant and its pt-BR line for every
//! string the screen shows; common words (Refresh, Search...) are in `super::Text`.

use super::Lang;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Text {
    Title,

    // Filter bar.
    FilterPeriod,
    AllTasks,
    AllStatuses,
    /// A task that no longer exists and has no execution left on the page to take its name from.
    UnknownTask,
    PeriodAll,
    PeriodLast24Hours,
    PeriodLast7Days,
    PeriodLast30Days,

    // Table columns.
    ColStatus,
    ColTrigger,
    ColStarted,
    ColDuration,
    ColExitCode,
    OpenExecution,

    // Pages.
    PagePrevious,
    PageNext,
    PageNumber,

    // States of the list.
    Loading,
    LoadFailed,
    Empty,
    EmptyHint,

    // Clearing the history of one task.
    ClearHistory,
    ClearTitle,
    ClearBody,
    ClearConfirm,
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
        Text::Title => "Histórico",

        Text::FilterPeriod => "Período",
        Text::AllTasks => "Todas",
        Text::AllStatuses => "Todos",
        Text::UnknownTask => "Tarefa removida",
        Text::PeriodAll => "Todo o período",
        Text::PeriodLast24Hours => "Últimas 24 h",
        Text::PeriodLast7Days => "Últimos 7 dias",
        Text::PeriodLast30Days => "Últimos 30 dias",

        Text::ColStatus => "Status",
        Text::ColTrigger => "Gatilho",
        Text::ColStarted => "Início",
        Text::ColDuration => "Duração",
        Text::ColExitCode => "Código de saída",
        Text::OpenExecution => "Ver",

        Text::PagePrevious => "Anterior",
        Text::PageNext => "Próxima",
        Text::PageNumber => "Página",

        Text::Loading => "Carregando…",
        Text::LoadFailed => "Não foi possível carregar o histórico.",
        Text::Empty => "Nenhuma execução encontrada.",
        Text::EmptyHint => "Se algum filtro estiver marcado, tente outro. O histórico também pode ter sido limpo.",

        Text::ClearHistory => "Limpar histórico da tarefa",
        Text::ClearTitle => "Limpar o histórico?",
        Text::ClearBody => {
            "Todas as execuções desta tarefa serão apagadas do histórico. Esta ação não pode ser desfeita."
        }
        Text::ClearConfirm => "Limpar",
    }
}
