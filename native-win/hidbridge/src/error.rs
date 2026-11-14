pub struct BridgeError {
    pub code: String,
    pub message: String,
}

pub type Result<T> = std::result::Result<T, BridgeError>;

pub fn failure<S: Into<String>, M: Into<String>>(code: S, message: M) -> BridgeError {
    BridgeError { code: code.into(), message: message.into() }
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::fmt::Debug for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for BridgeError {}
