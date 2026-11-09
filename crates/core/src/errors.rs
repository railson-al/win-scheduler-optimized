use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid task: {0}")]
    InvalidTask(String),
    #[error("invalid schedule: {0}")]
    InvalidSchedule(String),
    #[error("unknown value: {0}")]
    UnknownValue(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
