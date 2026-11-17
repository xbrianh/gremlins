#[derive(Debug)]
pub enum ClientError {
    Timeout { message: String },
    ApiServerError { message: String },
    RetryExhausted { message: String },
    Runtime { message: String },
    Cancelled,
    Bail { reason: String },
    PromptTooLarge { limit: u64, actual: u64 },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Timeout { message } => write!(f, "{message}"),
            ClientError::ApiServerError { message } => write!(f, "{message}"),
            ClientError::RetryExhausted { message } => write!(f, "{message}"),
            ClientError::Runtime { message } => write!(f, "{message}"),
            ClientError::Cancelled => write!(f, "cancelled"),
            ClientError::Bail { reason } => write!(f, "{reason}"),
            ClientError::PromptTooLarge { limit, actual } => write!(
                f,
                "prompt size {actual} bytes exceeds configured limit of {limit} bytes"
            ),
        }
    }
}

impl std::error::Error for ClientError {}
