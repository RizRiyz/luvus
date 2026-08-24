use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

pub type ApiResult<T = Value> = Result<T, ApiError>;

impl ApiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl From<ApiError> for (String, String) {
    fn from(error: ApiError) -> Self {
        (error.code, error.message)
    }
}
