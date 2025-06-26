// axum-realtime-kit/src/ws/types.rs

//! Internal types used by the `WebsocketService`.

use axum::extract::ws::{Message, WebSocket};
use dashmap::DashMap;
use futures_util::stream::SplitSink;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

/// A unique identifier for a single WebSocket connection.
pub type ConnectionId = Uuid;

/// A topic identifier, used for Redis Pub/Sub channels and subscription mapping.
pub type Topic = String;

/// A type alias for the WebSocket's writing half (the "Sink"),
/// protected by a Mutex for safe concurrent access from multiple tasks.
pub type WsSink = Arc<Mutex<SplitSink<WebSocket, Message>>>;

/// Defines the commands that can be sent to the background Redis listener task.
/// This allows the main service to dynamically change the listener's subscriptions.
#[derive(Debug, Clone)]
pub(crate) enum RedisCommand {
    /// Command to subscribe the Redis listener to a new topic.
    Subscribe(Topic),
    /// Command to unsubscribe the Redis listener from a topic.
    Unsubscribe(Topic),
}

/// Holds the shared state for the WebSocket service on a single server instance.
///
/// `DashMap` is used for high-performance, concurrent access without `async` locks.
#[derive(Default)]
pub(crate) struct WsState {
    /// Maps a `ConnectionId` to its corresponding `WsSink`.
    /// This allows for sending messages to a specific client.
    pub(crate) connections: DashMap<ConnectionId, WsSink>,

    /// Maps a `Topic` to a set of `ConnectionId`s that are subscribed to it.
    /// This is the core of the pub/sub logic on a single node.
    pub(crate) subscriptions: DashMap<Topic, HashSet<ConnectionId>>,
}

impl fmt::Debug for WsState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsState")
            .field("connections_count", &self.connections.len())
            .field("subscriptions_count", &self.subscriptions.len())
            .finish()
    }
}
