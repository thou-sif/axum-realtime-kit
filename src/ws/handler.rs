// axum-realtime-kit/src/ws/handler.rs

//! Defines the `MessageHandler` trait, the core abstraction for implementing
//! application-specific WebSocket logic.

use crate::ws::types::{ConnectionId, Topic, WsSink};
use async_trait::async_trait;
use axum::http::StatusCode;
use serde::{Serialize, de::DeserializeOwned};
use std::fmt::Debug;
use std::sync::Arc;
use serde_json::Value;

/// A context object passed to message handler methods.
///
/// It contains all the relevant state for a single connection, allowing the
/// handler to access application state, user information, and the client's
/// WebSocket sink for direct communication.
#[derive(Debug)]
pub struct ConnectionContext<S, U>
where
    S: Send + Sync + 'static,
    U: Send + Sync + 'static,
{
    /// The unique ID of the connection.
    pub conn_id: ConnectionId,
    /// The ID of the authenticated user. Its type is defined by the `MessageHandler`.
    pub user_id: U,
    /// The topic this connection is subscribed to.
    pub topic: Topic,
    /// A clone of the shared application state.
    pub app_state: Arc<S>,
    /// A clone of the WebSocket sink for sending messages directly to the client.
    pub sink: WsSink,
}

/// A standard error type for handler methods, consisting of an HTTP status code
/// and an optional, more specific error message.
#[derive(Debug)]
pub enum HandlerError {
    /// A custom error with a status code and message.
    Custom(StatusCode, Option<String>),
    /// An error that occurred during response serialization.
    Serialization(serde_json::Error),
}

impl From<(StatusCode, Option<String>)> for HandlerError {
    fn from(value: (StatusCode, Option<String>)) -> Self {
        HandlerError::Custom(value.0, value.1)
    }
}

// Allow easy conversion from serde_json::Error.
impl From<serde_json::Error> for HandlerError {
    fn from(value: serde_json::Error) -> Self {
        HandlerError::Serialization(value)
    }
}

/// The central trait that a user's application must implement.
///
/// This trait decouples the generic `WebsocketService` from any specific
/// business logic, allowing users to "plug in" their own message handling,
/// event types, and application state.
#[async_trait]
pub trait MessageHandler: Send + Sync + 'static {
    /// The type for messages coming from the client (e.g., a `WsClientMessage` enum).
    /// Must be deserializable from a string (e.g., JSON).
    type ClientMessage: DeserializeOwned + Send + Debug;

    /// The type for events broadcast via Redis (e.g., a `ServerEvent` enum).
    /// Must be serializable to a string (e.g., JSON).
    type ServerEvent: Serialize + Send + Sync + Debug;

    /// The shared application state (e.g., a struct holding database connection pools).
    type AppState: Send + Sync + 'static;

    /// The type for the user identifier. This can be `i64`, `Uuid`, `String`, etc.
    type UserId: Send + Sync + Clone + Debug + 'static;

    /// A lifecycle hook called immediately after a client successfully connects
    /// and is subscribed.
    ///
    /// The default implementation does nothing.
    async fn on_connect(&self, _context: &ConnectionContext<Self::AppState, Self::UserId>) {
        // Default is a no-op
    }

    /// Handles a message that expects a direct response to the sender, not a broadcast.
    ///
    /// This is ideal for read-only operations like "list items" or "get item".
    /// If `Ok(Some(response))` is returned, the `response` is serialized and sent
    /// directly to the client who sent the message.
    ///
    /// If `Ok(None)` is returned, the `WebsocketService` will proceed to call
    /// `handle_broadcast_message` with the same message. This allows for a single
    /// message to potentially trigger both a direct response and a broadcast.
    ///
    /// The default implementation returns `Ok(None)`, falling back to the broadcast handler.
    async fn handle_direct_message(
        &self,
        _msg: &Self::ClientMessage,
        _context: &ConnectionContext<Self::AppState, Self::UserId>,
    ) -> Result<Option<Value>, HandlerError> {
        Ok(None)
    }

    /// Handles a message that may result in an event being broadcast to all subscribers.
    ///
    /// This is ideal for state-changing operations like "create item" or "delete item".
    /// If `Ok(Some(event))` is returned, the `event` is serialized and published to
    //  Redis, which then broadcasts it to all clients on the topic (including the sender).
    ///
    /// If `Ok(None)` is returned, the message was handled successfully, but no event
    /// needs to be broadcast.
    async fn handle_broadcast_message(
        &self,
        msg: Self::ClientMessage,
        context: &ConnectionContext<Self::AppState, Self::UserId>,
    ) -> Result<Option<Self::ServerEvent>, HandlerError>;

    /// A lifecycle hook called after a client has disconnected.
    ///
    /// Useful for logging or performing cleanup related to the user's session.
    /// The default implementation does nothing.
    async fn on_disconnect(&self, _context: &ConnectionContext<Self::AppState, Self::UserId>) {
        // Default is a no-op
    }
}
