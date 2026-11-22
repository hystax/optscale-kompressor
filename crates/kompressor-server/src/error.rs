//! What a caller is told when a request cannot be served.
//!
//! Two rules shape every variant here. The first is that a body never reaches
//! an error message: payloads are tool output and can hold anything the command
//! printed, including credentials, and an error string is the one place that
//! travels furthest from the request — into the client's logs, an operator's
//! terminal, a bug report. The second is that the status has to be actionable:
//! a caller that gets 503 should send its prompt uncompressed, and a caller
//! that gets 422 should fix a key it spelled wrong, and neither can act on a
//! blanket 500 covering both.

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use kompressor_core::AuthError;
use thiserror::Error;

/// The code and message of an error, carried in the response extensions so the
/// envelope middleware can stamp the request id onto it.
///
/// The body is rendered twice — once here, once by the middleware — because an
/// [`ApiError`] returned from a handler that is *not* wrapped in our middleware
/// (a surface mounting one of our handlers in its own stack) must still produce
/// a valid body rather than an empty one.
#[derive(Clone, Debug)]
pub struct ErrorEnvelope {
    /// Stable machine-readable code: clients switch on this, not on the text.
    pub code: &'static str,
    /// Human-readable, and free of anything the caller sent as payload.
    pub message: String,
}

/// Everything a request can be refused with.
#[derive(Debug, Error)]
pub enum ApiError {
    /// The body is not JSON at all.
    #[error("{0}")]
    BadJson(String),
    /// No credential, or one we do not know.
    #[error("no valid credential presented")]
    Unauthorized,
    /// A credential we know and have switched off.
    #[error("this credential is disabled")]
    Forbidden,
    /// Over `server.max_body_bytes`.
    #[error("request body is larger than the configured limit")]
    TooLarge,
    /// Valid JSON that does not describe a request we can run: an unknown
    /// option key, an out-of-range knob, a message that is not an object.
    #[error("{0}")]
    Invalid(String),
    /// No engine permit inside `server.queue_timeout`.
    #[error("the compression engine is saturated; send the prompt uncompressed")]
    Saturated,
    /// Anything else. The detail is logged, not returned.
    #[error("internal error")]
    Internal(String),
    /// A stats or config lookup that found nothing.
    #[error("{0}")]
    NotFound(String),
    /// A store this server writes to off-host (S3-compatible object
    /// storage) refused or could not be reached. The message is the store's
    /// own reason and never carries a credential.
    #[error("{0}")]
    BadGateway(String),
}

impl ApiError {
    /// The status this maps to.
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::BadJson(_) => StatusCode::BAD_REQUEST,
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden => StatusCode::FORBIDDEN,
            ApiError::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ApiError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::Saturated => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::BadGateway(_) => StatusCode::BAD_GATEWAY,
        }
    }

    /// The stable code clients switch on.
    pub fn code(&self) -> &'static str {
        match self {
            ApiError::BadJson(_) => "bad_json",
            ApiError::Unauthorized => "unauthorized",
            ApiError::Forbidden => "forbidden",
            ApiError::TooLarge => "payload_too_large",
            ApiError::Invalid(_) => "invalid_request",
            ApiError::Saturated => "engine_saturated",
            ApiError::Internal(_) => "internal_error",
            ApiError::NotFound(_) => "not_found",
            ApiError::BadGateway(_) => "storage_unavailable",
        }
    }

    /// What the caller is told.
    ///
    /// [`ApiError::Internal`] carries its detail for the log and deliberately
    /// does not return it: an internal failure's text is the most likely place
    /// for a path, a query or a fragment of a payload to escape.
    pub fn public_message(&self) -> String {
        match self {
            ApiError::Internal(_) => "internal error".to_string(),
            other => other.to_string(),
        }
    }

    /// The envelope for this error.
    pub fn envelope(&self) -> ErrorEnvelope {
        ErrorEnvelope {
            code: self.code(),
            message: self.public_message(),
        }
    }
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> ApiError {
        match error {
            // A missing credential and an unknown one are the same answer on
            // purpose: telling a caller which one it got is telling it whether
            // a guessed key exists.
            AuthError::Missing | AuthError::Unknown => ApiError::Unauthorized,
            AuthError::Disabled => ApiError::Forbidden,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let ApiError::Internal(detail) = &self {
            tracing::error!(detail = %detail, "request failed");
        }
        let status = self.status();
        let envelope = self.envelope();
        let body = render(&envelope, None);
        let mut response = (status, body).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        if matches!(self, ApiError::Saturated) {
            // The whole point of a 503 here is that the caller does something
            // else — sends the prompt uncompressed — rather than retrying into
            // a queue that is already full, so the hint is short on purpose.
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
        }
        response.extensions_mut().insert(envelope);
        response
    }
}

/// Render an envelope as the body every error response carries.
pub fn render(envelope: &ErrorEnvelope, request_id: Option<&str>) -> String {
    let value = serde_json::json!({
        "error": {
            "code": envelope.code,
            "message": envelope.message,
            "request_id": request_id,
        }
    });
    value.to_string()
}

/// Turn a body that is not JSON into a 400.
pub fn bad_json(error: &serde_json::Error) -> ApiError {
    ApiError::BadJson(format!(
        "malformed JSON at line {} column {}",
        error.line(),
        error.column()
    ))
}

/// Turn JSON that does not match a schema into a 422.
///
/// serde's own text is only reused when it names a *key*: "unknown field",
/// "missing field" and "duplicate field" quote the caller's own field name,
/// which is exactly what a caller needs to fix a typo. Every other data error
/// ("invalid type: string \"hunter2\", expected u64") quotes the caller's
/// *value*, which is payload, so those are reduced to a position.
pub fn schema_error(error: &serde_json::Error) -> ApiError {
    let text = error.to_string();
    let names_a_key = text.starts_with("unknown field")
        || text.starts_with("missing field")
        || text.starts_with("duplicate field");
    if names_a_key {
        // Trim serde's trailing " at line L column C" only when it is there;
        // the field name is the useful half and the position adds nothing.
        let message = text
            .split(" at line ")
            .next()
            .unwrap_or(&text)
            .trim()
            .to_string();
        return ApiError::Invalid(message);
    }
    ApiError::Invalid(format!(
        "request does not match the schema, at line {} column {}",
        error.line(),
        error.column()
    ))
}

/// Parse a body, distinguishing "not JSON" from "not this shape".
pub fn parse_body<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
    match serde_json::from_slice::<T>(bytes) {
        Ok(value) => Ok(value),
        Err(error) if error.classify() == serde_json::error::Category::Data => {
            Err(schema_error(&error))
        }
        Err(error) => Err(bad_json(&error)),
    }
}

/// The envelope for a status nothing in this crate produced: a 404 from the
/// router, a 413 from the body limit layer, a 408 from the timeout.
pub fn envelope_for_status(status: StatusCode) -> ErrorEnvelope {
    let (code, message): (&'static str, &str) = match status {
        StatusCode::NOT_FOUND => ("not_found", "no such endpoint"),
        StatusCode::METHOD_NOT_ALLOWED => ("method_not_allowed", "method not allowed here"),
        StatusCode::PAYLOAD_TOO_LARGE => (
            "payload_too_large",
            "request body is larger than the configured limit",
        ),
        StatusCode::REQUEST_TIMEOUT => ("timeout", "the request took longer than the timeout"),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => ("unsupported_media_type", "expected JSON"),
        StatusCode::BAD_REQUEST => ("bad_request", "the request could not be read"),
        _ => ("error", "the request could not be served"),
    };
    ErrorEnvelope {
        code,
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Shape {
        #[allow(dead_code, reason = "the field exists to be mistyped at, not read")]
        wanted: u64,
    }

    #[test]
    fn a_body_that_is_not_json_is_a_400_and_a_body_of_the_wrong_shape_is_a_422() {
        let broken = parse_body::<Shape>(b"{not json").unwrap_err();
        assert_eq!(broken.status(), StatusCode::BAD_REQUEST);
        let wrong = parse_body::<Shape>(br#"{"wantd": 1}"#).unwrap_err();
        assert_eq!(wrong.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn a_schema_error_names_the_key_the_caller_got_wrong_but_never_the_value() {
        let unknown = parse_body::<Shape>(br#"{"wantd": 1}"#).unwrap_err();
        assert!(
            unknown.to_string().contains("wantd"),
            "the typo is nameable"
        );

        // a mistyped value is payload: "hunter2" must not come back out
        let mistyped = parse_body::<Shape>(br#"{"wanted": "hunter2"}"#).unwrap_err();
        assert_eq!(mistyped.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!mistyped.to_string().contains("hunter2"));
    }

    #[test]
    fn an_internal_error_keeps_its_detail_out_of_the_response() {
        let error = ApiError::Internal("/var/lib/kompressor/refs.db is locked".into());
        assert_eq!(error.public_message(), "internal error");
        assert!(!error.envelope().message.contains("refs.db"));
    }

    #[test]
    fn a_missing_credential_and_an_unknown_one_are_indistinguishable() {
        assert_eq!(
            ApiError::from(AuthError::Missing).status(),
            ApiError::from(AuthError::Unknown).status()
        );
        assert_eq!(
            ApiError::from(AuthError::Disabled).status(),
            StatusCode::FORBIDDEN
        );
    }
}
