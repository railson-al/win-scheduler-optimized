//! Texts of the execution and log screen (`views/log_view.rs`). Add a `Text` variant and its pt-BR line
//! for every string the screen shows; common words (Copy, Stop...) are in `super::Text`.
//!
//! Texts with a value in them carry a `{name}` placeholder that the screen fills in.

use super::Lang;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Text {
    Title,
    NoExecutionOpen,
    Back,
    Loading,
    NotFound,
    /// A field with no value yet (not started, not finished, no exit code).
    NoValue,

    // Header.
    Status,
    Trigger,
    ScheduledFor,
    Started,
    Finished,
    Duration,
    ExitCode,
    LogSize,
    FailureReason,
    LogTruncated,

    // Following the log.
    Live,
    Complete,
    RetryFollowing,

    // Actions.
    ExportToFile,
    ExportPending,
    FileFilterLog,
    /// `{count}` lines left the screen.
    DroppedLines,
    /// `{path}` and `{size}`.
    Exported,
    /// `{reason}`.
    ExportFailed,

    // Filters.
    ShowOutput,
    ShowErrors,
    ShowMeta,
    KeepScrolled,

    // The body.
    NoLines,
    NoLinesMatch,

    /// The start of a suggested file name, when the task has no usable name.
    DefaultFileName,
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
        Text::Title => "Execução",
        Text::NoExecutionOpen => "Nenhuma execução aberta.",
        Text::Back => "Voltar",
        Text::Loading => "Carregando…",
        Text::NotFound => "Execução não encontrada.",
        Text::NoValue => "—",

        Text::Status => "Status",
        Text::Trigger => "Gatilho",
        Text::ScheduledFor => "Agendada para",
        Text::Started => "Início",
        Text::Finished => "Fim",
        Text::Duration => "Duração",
        Text::ExitCode => "Código de saída",
        Text::LogSize => "Tamanho do log",
        Text::FailureReason => "Motivo da falha",
        Text::LogTruncated => "O log foi truncado pelo limite de tamanho.",

        Text::Live => "acompanhando ao vivo",
        Text::Complete => "log completo",
        Text::RetryFollowing => "Tentar de novo",

        Text::ExportToFile => "Exportar…",
        Text::ExportPending => "Escolha onde salvar o arquivo…",
        Text::FileFilterLog => "Arquivo de log",
        Text::DroppedLines => {
            "As {count} primeiras linhas saíram da tela para poupar memória; o log completo está no arquivo (use Exportar)."
        }
        Text::Exported => "Exportado para {path} ({size})",
        Text::ExportFailed => "Não foi possível exportar: {reason}",

        Text::ShowOutput => "Saída",
        Text::ShowErrors => "Erros",
        Text::ShowMeta => "ScriptRunner",
        Text::KeepScrolled => "Rolar até o fim",

        Text::NoLines => "Ainda não há linhas.",
        Text::NoLinesMatch => "Nenhuma linha com os filtros escolhidos.",

        Text::DefaultFileName => "execucao",
    }
}
