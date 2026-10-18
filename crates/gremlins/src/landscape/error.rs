#[derive(Debug, thiserror::Error)]
pub enum LandscapeError {
    #[error("failed to read file: {0}")]
    Read(#[from] std::io::Error),

    #[error("file is not valid UTF-8: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    #[error(
        "landscape block cannot fit within the size budget even after truncating every item name"
    )]
    BudgetExceeded,
}
