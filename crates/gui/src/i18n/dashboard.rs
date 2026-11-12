//! Texts of the task list screen (`views/dashboard.rs`). Add a `Text` variant and its pt-BR line for
//! every string the screen shows; common words (Run, Stop, Edit, Delete...) are in `super::Text`.

use super::Lang;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Text {
    Title,
    SearchHint,
    NewTask,
    TaskOne,
    TaskMany,
    RunningLower,
    Loading,
    DisconnectedHint,
    NoTasks,
    NoTasksHint,
    NoMatches,
    NoMatchesHint,
    ColActions,
    NoValue,
    DeleteTitle,
    DeleteBefore,
    DeleteAfter,
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
        Text::Title => "Tarefas",
        Text::SearchHint => "Nome da tarefa",
        Text::NewTask => "Nova tarefa",
        Text::TaskOne => "tarefa",
        Text::TaskMany => "tarefas",
        Text::RunningLower => "executando",
        Text::Loading => "Carregando tarefas…",
        Text::DisconnectedHint => "A lista aparece quando a conexão com o serviço voltar.",
        Text::NoTasks => "Nenhuma tarefa ainda",
        Text::NoTasksHint => {
            "Clique em “Nova tarefa” para criar a primeira: escolha um script e quando ele deve rodar."
        }
        Text::NoMatches => "Nenhuma tarefa corresponde à busca.",
        Text::NoMatchesHint => "Confira o texto digitado ou limpe o campo de busca.",
        Text::ColActions => "Ações",
        Text::NoValue => "—",
        Text::DeleteTitle => "Excluir tarefa?",
        Text::DeleteBefore => "A tarefa",
        Text::DeleteAfter => "será excluída. Esta ação não pode ser desfeita.",
    }
}
