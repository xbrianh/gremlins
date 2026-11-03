#[derive(Debug, thiserror::Error)]
pub enum LandscapeError {
    #[error("failed to read file: {0}")]
    Read(#[from] std::io::Error),

    #[error("file is not valid UTF-8: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    #[error("landscape block cannot fit within the token budget even at minimum depth")]
    BudgetExceeded,

    #[error("tokenizer initialization failed: {0}")]
    Tokenizer(String),
}
