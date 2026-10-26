use rig_core::completion::Message;

#[derive(Debug)]
pub enum ClientError {
    Timeout {
        message: String,
    },
    ApiServerError {
        message: String,
    },
    RetryExhausted {
        message: String,
    },
    Runtime {
        message: String,
    },
    Cancelled {
        message: String,
        /// The agent loop's accumulated message history at the moment of
        /// cancellation, when available. Chat soft-interrupt recovery uses
        /// this to reconstruct the partial turn before re-running.
        messages: Option<Vec<Message>>,
    },
    Bail {
        reason: String,
    },
    PromptTooLarge {
        limit: u64,
        actual: u64,
    },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Timeout { message } => write!(f, "{message}"),
            ClientError::ApiServerError { message } => write!(f, "{message}"),
            ClientError::RetryExhausted { message } => write!(f, "{message}"),
            ClientError::Runtime { message } => write!(f, "{message}"),
            ClientError::Cancelled { message, .. } => write!(f, "{message}"),
            ClientError::Bail { reason } => write!(f, "{reason}"),
            ClientError::PromptTooLarge { limit, actual } => write!(
                f,
                "prompt size {actual} bytes exceeds configured limit of {limit} bytes"
            ),
        }
    }
}

impl std::error::Error for ClientError {}
