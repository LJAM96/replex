use salvo::http::StatusCode;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("authentication failed: {0}")]
    Authentication(String),
    #[error("request is forbidden by policy: {0}")]
    Forbidden(String),
    #[error("invalid client input: {0}")]
    InvalidClientInput(String),
    #[error("upstream transport failed: {0}")]
    UpstreamTransport(String),
    #[error("upstream response could not be parsed: {0}")]
    UpstreamParse(String),
    #[error("configuration error: {0}")]
    Configuration(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::Authentication(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::InvalidClientInput(_) => StatusCode::BAD_REQUEST,
            Self::UpstreamTransport(_) | Self::UpstreamParse(_) => {
                StatusCode::BAD_GATEWAY
            }
            Self::Configuration(_) | Self::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_classes_have_stable_http_statuses() {
        assert_eq!(
            AppError::Authentication("x".into()).status_code(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            AppError::Forbidden("x".into()).status_code(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            AppError::InvalidClientInput("x".into()).status_code(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            AppError::UpstreamTransport("x".into()).status_code(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            AppError::UpstreamParse("x".into()).status_code(),
            StatusCode::BAD_GATEWAY
        );
    }
}
