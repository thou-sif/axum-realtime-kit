//! ## Example
//!
//! ```rust,no_run
//! # use async_trait::async_trait;
//! # use axum::{response::Response, routing::get, Router, response::IntoResponse};
//! # use axum_realtime_kit::auth::{TokenValidator, WsAuth};
//! # use std::{sync::Arc, net::SocketAddr};
//! #
//! // 1. Your custom user struct
//! #[derive(Debug, Clone)]
//! struct User {
//!     id: i64,
//!     username: String,
//! }
//!
//! // Your application's shared state
//! #[derive(Clone)]
//! struct AppState {
//!     // ... your db pools, etc.
//! }
//!
//! // 2. Implement the trait on your state
//! #[async_trait]
//! impl TokenValidator for AppState {
//!     type User = User;
//!     type Error = std::io::Error;
//!
//!     async fn validate_token(&self, token: &str) -> Result<Self::User, Self::Error> {
//!         // ...
//! #        if token == "secret-token" {
//! #            Ok(User { id: 123, username: "Alice".to_string() })
//! #        } else {
//! #            Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid token"))
//! #        }
//!     }
//! }
//!
//! // 3. Use the extractor in your handler
//! async fn websocket_handler(
//!     auth: WsAuth<User>,
//! ) -> Response {
//! let user: User = auth.0;
//!     println!("Authenticated user: {:?}", user);
//!     "Hello!".into_response()
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     // The AppState itself is the state. It needs to be Clone.
//!     let app_state = AppState {};
//!     let app: Router<AppState> = Router::new()
//!         .route("/ws", get(websocket_handler))
//!         .with_state(app_state);
//!     // ...
//! }
//! ```

use async_trait::async_trait;
use axum::{
    extract::{FromRequestParts, Query},
    http::{HeaderMap, StatusCode, request::Parts},
    response::{IntoResponse, Response},
};
use serde::Deserialize;

/// The public trait the user's `AppState` must implement to enable `WsAuth`.
///
/// This trait decouples the authentication mechanism from the extractor, allowing
/// any token validation strategy to be used.
#[async_trait]
pub trait TokenValidator {
    /// The user type that is returned upon successful validation. This can be any
    /// struct that is `Send + Sync + 'static`.
    type User: Send + Sync + 'static;
    /// The error type returned on validation failure.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Validates a token string and returns a user on success.
    ///
    /// This is where you implement your specific authentication logic, such as
    /// decoding a JWT or querying a database.
    ///
    /// # Arguments
    /// * `token` - The token string extracted from the request.
    async fn validate_token(&self, token: &str) -> Result<Self::User, Self::Error>;
}

/// The generic WebSocket authentication extractor.
///
/// This extractor requires the request to be authenticated. It holds the validated
/// user object of type `U`. If authentication fails, the request is rejected with
//  a `401 Unauthorized` response.
#[derive(Debug)]
pub struct WsAuth<U>(pub U)
where
    U: Send + Sync + 'static;

/// The query parameter struct used internally for token extraction.
#[derive(Deserialize)]
struct WebSocketAuthQuery {
    token: String,
}

impl<S, U> FromRequestParts<S> for WsAuth<U>
where
    S: TokenValidator<User = U> + Send + Sync + 'static,
    U: Send + Sync + 'static,
{
    type Rejection = Response;

    fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> impl Future<Output = Result<Self, <Self as FromRequestParts<S>>::Rejection>> + Send {
        Box::pin(async move {
            // Extract token from header or query
            let token = get_token_from_headers(&parts.headers);
            let token = if let Some(t) = token {
                Some(t)
            } else {
                match Query::<WebSocketAuthQuery>::from_request_parts(parts, state).await {
                    Ok(Query(q)) => Some(q.token),
                    Err(_) => None,
                }
            };

            let token = match token {
                Some(t) => t,
                None => return Err(StatusCode::UNAUTHORIZED.into_response()),
            };

            match state.validate_token(&token).await {
                Ok(user) => Ok(WsAuth(user)),
                Err(_) => Err(StatusCode::UNAUTHORIZED.into_response()),
            }
        })
    }
}

/// A private helper function to extract a bearer token from the Authorization header.
fn get_token_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get("Authorization")
        .and_then(|header| header.to_str().ok())
        .and_then(|header_val| {
            header_val
                .strip_prefix("Bearer ")
                .map(|token| token.to_owned())
        })
}
