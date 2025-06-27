//! Provides the generic Axum handler for upgrading HTTP requests to WebSockets.

use crate::ws::handler::MessageHandler;
use crate::ws::service::WebsocketService;
use axum::{
    extract::{State, ws::WebSocketUpgrade},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::future::Future;
use std::sync::Arc;
use tracing::{error, instrument};

/// A generic Axum handler that orchestrates the WebSocket upgrade process.
///
/// This handler should be used directly in your Axum router. It performs the
/// following steps in order:
///
/// 1.  **(Auth Feature)** Authenticates the request using the `WsAuth` extractor.
/// 2.  Parses path parameters to determine the topic.
/// 3.  Runs the application-specific `validator` closure to authorize the user for the topic.
/// 4.  If all checks pass, it upgrades the connection and hands it off to the `WebsocketService`.
///
/// ## Path Parameters
///
/// This handler assumes a path with a single parameter that will be used as the
/// WebSocket topic, for example: `/ws/topic-name`. For more complex paths like
/// `/ws/type/{type_id}/id/{resource_id}`, you should create a simple wrapper
/// handler that extracts the parameters, constructs the topic string, and then
/// calls this handler.
///
/// ## Example Usage
///
/// ```rust,no_run
/// # use axum::{Router, routing::get, response::IntoResponse, http::StatusCode};
/// # use std::sync::Arc;
/// # use axum_realtime_kit::ws::{
/// #   handler::{MessageHandler, ConnectionContext, HandlerError},
/// #   service::WebsocketService,
/// #   upgrade::upgrade_handler
/// # };
/// # use axum_realtime_kit::auth::{TokenValidator, WsAuth};
/// # use serde::{Serialize, Deserialize};
/// # use async_trait::async_trait;
/// #
/// # #[derive(Debug, Serialize, Deserialize)] struct MyMessage;
/// # #[derive(Debug, Serialize, Deserialize)] struct MyEvent;
/// # #[derive(Debug, Clone)] struct MyUser { id: i64 }
/// # #[derive(Clone)] struct AppState;
/// #
/// # #[async_trait]
/// # impl TokenValidator for AppState {
/// #   type User = MyUser; type Error = std::io::Error;
/// #   async fn validate_token(&self, t: &str) -> Result<Self::User, Self::Error> {
/// #     Ok(MyUser { id: 123 })
/// #   }
/// # }
/// #
/// # struct MyHandler;
/// # #[async_trait]
/// # impl MessageHandler for MyHandler {
/// #   type ClientMessage = MyMessage; type ServerEvent = MyEvent;
/// #   type AppState = AppState; type UserId = i64;
/// #   async fn handle_broadcast_message(
/// #       &self, msg: MyMessage, ctx: &ConnectionContext<AppState, i64>
/// #   ) -> Result<Option<MyEvent>, HandlerError> { Ok(None) }
/// # }
///
/// // Your application-specific validation logic.
/// async fn validate_topic_access(
///     app_state: Arc<AppState>,
///     user_id: i64,
///     topic: String,
/// ) -> Result<(), StatusCode> {
///     // In a real app, you'd check a database here.
///     // e.g., "does this user have permission to access this chat room?"
///     if topic.starts_with("private-") && user_id != 123 {
///         return Err(StatusCode::FORBIDDEN);
///     }
///     Ok(())
/// }
///
/// #[tokio::main]
/// async fn main() {
///     use axum::extract::{Path, State, WebSocketUpgrade};
///     #[derive(Clone)]
///     struct CombinedState {
///        app_state: Arc<AppState>,
///        ws_service: Arc<WebsocketService<MyHandler>>,
///     }
///     #[async_trait]
///     impl TokenValidator for CombinedState {
///         type User = MyUser;
///         type Error = <AppState as TokenValidator>::Error;
///         
///        async fn validate_token(&self, token: &str) -> Result<Self::User, Self::Error> {
///             self.app_state.validate_token(token).await
///         }
///     }
///
///     let app_state = Arc::new(AppState {});
///     let handler = MyHandler {};
///     let ws_service = WebsocketService::new("redis://127.0.0.1/", handler, app_state.clone());
///     
///     let combined_state = CombinedState {
///         app_state: app_state.clone(),
///         ws_service: ws_service,
///     };
///
///     // The generic WebsocketService instantiated with your types.
///     let app: Router<CombinedState> = Router::new()
///         .route(
///             "/ws/:topic",
///             get(|
///                 ws: WebSocketUpgrade,
///                 Path(topic): Path<String>,
///                 State(state): State<CombinedState>,
///                 auth: WsAuth<MyUser>,
///             | async move {
///                 let user_id = auth.0.id;
///                 // Use the ws_service from the combined state
///                 upgrade_handler(
///                     ws,
///                     State(state.ws_service),
///                     user_id,
///                     topic,
///                     |app_state, user_id, topic| validate_topic_access(app_state, user_id, topic)
///                 ).await
///             })
///         )
///         .with_state(combined_state);
///     // ... serve the app
/// }
/// ```
#[instrument(skip_all, fields(user_id = ?user_id, topic = %topic))]
pub async fn upgrade_handler<H, V, F>(
    ws: WebSocketUpgrade,
    State(service): State<Arc<WebsocketService<H>>>,
    user_id: H::UserId,
    topic: String,
    validator: V,
) -> Response
where
    H: MessageHandler,
    V: FnOnce(Arc<H::AppState>, H::UserId, String) -> F,
    F: Future<Output = Result<(), StatusCode>>,
{
    // 1. Run the application-specific validation logic.
    let validation_result =
        validator(service.app_state.clone(), user_id.clone(), topic.clone()).await;

    if let Err(status_code) = validation_result {
        error!(
            "WebSocket connection rejected by validator with status: {}",
            status_code
        );
        return status_code.into_response();
    }

    // 2. All checks passed. Upgrade the connection.
    // The `on_upgrade` callback runs in the background.
    ws.on_upgrade(move |socket| async move {
        // The WebsocketService takes over from here.
        service.handle_connection(socket, topic, user_id).await;
    })
}
