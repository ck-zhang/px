use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionOutcome {
    pub status: CommandStatus,
    pub message: String,
    #[serde(default)]
    pub details: Value,
}

impl ExecutionOutcome {
    pub fn success(message: impl Into<String>, details: Value) -> Self {
        Self {
            status: CommandStatus::Ok,
            message: message.into(),
            details,
        }
    }

    pub fn failure(message: impl Into<String>, details: Value) -> Self {
        Self {
            status: CommandStatus::Failure,
            message: message.into(),
            details,
        }
    }

    pub fn user_error(message: impl Into<String>, details: Value) -> Self {
        Self {
            status: CommandStatus::UserError,
            message: message.into(),
            details,
        }
    }
}

#[derive(thiserror::Error, Debug)]
#[error("{message}")]
pub struct InstallUserError {
    pub(crate) message: String,
    pub(crate) details: Value,
}

impl InstallUserError {
    pub fn new(message: impl Into<String>, details: Value) -> Self {
        Self {
            message: message.into(),
            details,
        }
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub fn details(&self) -> &Value {
        &self.details
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CommandStatus {
    Ok,
    UserError,
    Failure,
}
