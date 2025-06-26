//! The primary `WebsocketService` that manages connections and state.

use crate::ws::{
    handler::{ConnectionContext, MessageHandler, HandlerError},
    types::{ConnectionId, RedisCommand, Topic, WsSink, WsState},
};
use axum::extract::ws::{Message, Utf8Bytes, WebSocket};
use futures_util::{
    SinkExt,
    stream::{SplitStream, StreamExt},
};
use redis::aio::PubSub;
use serde::Serialize;
use std::fmt::Debug;
use std::sync::Arc;
use redis::AsyncCommands;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, error, info, instrument, warn};

/// The main service that manages WebSocket connections and Redis Pub/Sub.
///
/// It is generic over a `MessageHandler` implementation, allowing it to be
/// adapted to any application's business logic.
#[derive(Debug)]
pub struct WebsocketService<H: MessageHandler> {
    state: Arc<WsState>,
    redis_cmd_tx: mpsc::Sender<RedisCommand>,
    redis_pub_client: redis::Client,
    handler: Arc<H>,
    pub(crate) app_state: Arc<H::AppState>,
}

impl<H: MessageHandler> Clone for WebsocketService<H> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            redis_cmd_tx: self.redis_cmd_tx.clone(),
            redis_pub_client: self.redis_pub_client.clone(),
            handler: self.handler.clone(),
            app_state: self.app_state.clone(),
        }
    }
}

impl<H: MessageHandler> WebsocketService<H> {
    /// Creates a new `WebsocketService` and spawns the background Redis listener.
    ///
    /// # Arguments
    /// * `redis_url` - The connection string for the Redis server.
    /// * `handler` - An instance of your application's `MessageHandler` implementation.
    /// * `app_state` - The shared application state.
    pub fn new(redis_url: &str, handler: H, app_state: Arc<H::AppState>) -> Arc<Self> {
        let redis_pub_client =
            redis::Client::open(redis_url).expect("Failed to create Redis client for publishing");
        let redis_sub_client =
            redis::Client::open(redis_url).expect("Failed to create Redis client for subscribing");

        let (redis_cmd_tx, redis_cmd_rx) = mpsc::channel(100);

        let service = Arc::new(Self {
            state: Arc::new(WsState::default()),
            redis_cmd_tx,
            redis_pub_client,
            handler: Arc::new(handler),
            app_state,
        });

        info!("Spawning Redis Pub/Sub listener task...");
        let service_clone = Arc::clone(&service);
        tokio::spawn(async move {
            service_clone
                .run_redis_listener(redis_sub_client, redis_cmd_rx)
                .await;
        });

        service
    }

    /// Public entry point called by the Axum handler for each new connection.
    #[instrument(skip_all, fields(conn_id, topic, user_id = ?user_id))]
    pub async fn handle_connection(
        self: Arc<Self>,
        socket: WebSocket,
        topic: Topic,
        user_id: H::UserId,
    ) {
        let conn_id = ConnectionId::new_v4();
        tracing::Span::current().record("conn_id", &tracing::field::display(conn_id));

        let (sink, stream) = socket.split();
        let sink = Arc::new(Mutex::new(sink));

        self.state.connections.insert(conn_id, Arc::clone(&sink));
        self.subscribe(conn_id, topic.clone()).await;

        let context = ConnectionContext {
            conn_id,
            user_id,
            topic,
            app_state: self.app_state.clone(),
            sink: sink.clone(),
        };

        info!("Client connected and subscribed.");
        self.handler.on_connect(&context).await;

        let service_clone = Arc::clone(&self);
        tokio::spawn(async move {
            service_clone
                .run_client_message_receiver(stream, context)
                .await
        });
    }

    /// Dedicated task that runs for each client, processing their incoming messages.
    #[instrument(skip(self, stream, context), fields(conn_id = ?context.conn_id, topic = %context.topic))]
    async fn run_client_message_receiver(
        &self,
        mut stream: SplitStream<WebSocket>,
        context: ConnectionContext<H::AppState, H::UserId>,
    ) {
        info!("Starting message receiver loop for client.");
        while let Some(Ok(msg)) = stream.next().await {
            if let Message::Text(text) = msg {
                match serde_json::from_str::<H::ClientMessage>(&text) {
                    Ok(parsed_message) => {
                        debug!(message = ?parsed_message, "Received message from client");
                        self.route_message(parsed_message, &context).await;
                    }
                    Err(e) => {
                        warn!("Failed to parse message from client: {}", e);
                        self.send_error_response("Invalid message format", &context.sink)
                            .await;
                    }
                }
            } else if let Message::Close(_) = msg {
                debug!("Received close frame from client.");
                break;
            }
        }
        self.on_disconnect(context).await;
    }

    /// Routes a parsed message to the appropriate handler method.
    async fn route_message(
        &self,
        msg: H::ClientMessage,
        context: &ConnectionContext<H::AppState, H::UserId>,
    ) {
        // A helper to handle sending error responses
        async fn handle_err<H: MessageHandler>(
            service: &WebsocketService<H>,
            e: HandlerError,
            sink: &WsSink,
        ) {
            let err_msg = match e {
                HandlerError::Custom(code, msg) => msg.unwrap_or_else(|| code.to_string()),
                HandlerError::Serialization(e) => format!("Response serialization error: {}", e),
            };
            service.send_error_response(&err_msg, sink).await;
        }

        // 1. Try the direct message handler first.
        match self.handler.handle_direct_message(&msg, context).await {
            Ok(Some(response_value)) => { // <--- RECEIVES A `serde_json::Value`
                if self.send_json_to_sink(&response_value, &context.sink).await.is_err() {
                    warn!("Failed to send direct response to client.");
                }
                return; // Message handled, do not fall through.
            }
            Err(e) => {
                handle_err(self, e, &context.sink).await;
                return; // Error occurred, do not fall through.
            }
            Ok(None) => {
                // No direct response, fall through to the broadcast handler.
            }
        }

        // 2. Fall back to the broadcast message handler.
        match self
            .handler
            .handle_broadcast_message(msg, context)
            .await
        {
            Ok(Some(event_to_publish)) => {
                if let Err(e) = self.publish_event(&context.topic, &event_to_publish).await {
                    error!("Failed to publish event after handling message: {}", e);
                    self.send_error_response("Failed to broadcast event", &context.sink).await;
                }
            }
            Ok(None) => { /* Handled successfully, but nothing to broadcast. */ }
            Err(e) => {
                handle_err(self, e, &context.sink).await;
            }
        }
    }

    /// The core background task that listens to Redis and manages its own subscriptions.
    async fn run_redis_listener(
        self: Arc<Self>,
        redis_client: redis::Client,
        mut cmd_rx: mpsc::Receiver<RedisCommand>,
    ) {
        loop {
            let mut pubsub_conn = match redis_client.get_async_pubsub().await {
                Ok(conn) => {
                    info!("Redis Pub/Sub listener connected successfully.");
                    conn
                }
                Err(e) => {
                    error!(
                        "Failed to connect to Redis for Pub/Sub: {}. Retrying in 5s.",
                        e
                    );
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            let topics_to_resubscribe: Vec<String> = self
                .state
                .subscriptions
                .iter()
                .map(|e| e.key().clone())
                .collect();

            if !topics_to_resubscribe.is_empty() {
                if let Err(e) = pubsub_conn.subscribe(&topics_to_resubscribe).await {
                    error!(
                        "Failed to resubscribe to Redis topics: {}. Will retry connection.",
                        e
                    );
                    continue;
                }
            }

            'event_loop: loop {
                let mut msg_stream = pubsub_conn.on_message();

                loop {
                    tokio::select! {
                        biased;
                        Some(command) = cmd_rx.recv() => {
                            drop(msg_stream);
                            if self.handle_redis_command(&mut pubsub_conn, command).await.is_err() {
                                break 'event_loop;
                            }
                            continue 'event_loop;
                        }
                        Some(msg) = msg_stream.next() => {
                            self.handle_redis_broadcast(msg).await;
                        }
                        else => {
                            warn!("Redis listener command channel closed. Shutting down.");
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Helper for the listener to execute subscribe/unsubscribe commands.
    async fn handle_redis_command(
        &self,
        pubsub_conn: &mut PubSub,
        command: RedisCommand,
    ) -> Result<(), redis::RedisError> {
        match command {
            RedisCommand::Subscribe(topic) => {
                info!(?topic, "Listener subscribing to topic");
                pubsub_conn.subscribe(&topic).await
            }
            RedisCommand::Unsubscribe(topic) => {
                info!(?topic, "Listener unsubscribing from topic");
                pubsub_conn.unsubscribe(&topic).await
            }
        }
    }

    /// Handles a broadcast message received from a Redis channel.
    async fn handle_redis_broadcast(&self, msg: redis::Msg) {
        let topic: Topic = msg.get_channel_name().to_string();
        let payload: String = match msg.get_payload() {
            Ok(p) => p,
            Err(e) => {
                error!(?topic, "Failed to get payload from Redis message: {}", e);
                return;
            }
        };

        if let Some(subscribers) = self.state.subscriptions.get(&topic) {
            let conn_ids: Vec<ConnectionId> = subscribers.value().iter().cloned().collect();
            drop(subscribers);

            if conn_ids.is_empty() {
                return;
            }

            debug!(
                ?topic,
                count = conn_ids.len(),
                "Broadcasting Redis message to clients"
            );

            let message = Message::Text(Utf8Bytes::from(payload));
            for conn_id in conn_ids {
                if let Some(sink_entry) = self.state.connections.get(&conn_id) {
                    let sink = Arc::clone(sink_entry.value());
                    let msg_clone = message.clone();
                    tokio::spawn(async move {
                        if let Err(e) = sink.lock().await.send(msg_clone).await {
                            warn!(
                                ?conn_id,
                                "Failed to send broadcast message, client likely disconnected: {}",
                                e
                            );
                        }
                    });
                }
            }
        }
    }

    /// Publishes a serializable event to a Redis channel.
    async fn publish_event(&self, topic: &str, event: &H::ServerEvent) -> Result<(), String> {
        let event_json = serde_json::to_string(event)
            .map_err(|e| format!("Failed to serialize event: {}", e))?;

        let mut conn = self
            .redis_pub_client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| format!("Failed to get Redis connection: {}", e))?;

        let receivers: usize = conn
            .publish(topic, &event_json)
            .await
            .map_err(|e| format!("Failed to publish to Redis: {}", e))?;

        debug!(
            ?topic,
            ?event,
            ?receivers,
            "Successfully published event to Redis"
        );

        Ok(())
    }

    /// Subscribes a connection, sending a command to the listener if it's the first on the node.
    async fn subscribe(&self, conn_id: ConnectionId, topic: Topic) {
        let mut entry = self.state.subscriptions.entry(topic.clone()).or_default();
        let is_first_subscriber_on_node = entry.is_empty();
        entry.insert(conn_id);
        drop(entry);

        if is_first_subscriber_on_node {
            info!(
                ?topic,
                "First subscriber on this node. Sending Subscribe command."
            );
            if let Err(e) = self.redis_cmd_tx.send(RedisCommand::Subscribe(topic)).await {
                error!("Failed to send Subscribe command to listener: {}", e);
            }
        }
    }

    /// Cleans up on disconnect, sending a command to the listener if it's the last on the node.
    async fn on_disconnect(&self, context: ConnectionContext<H::AppState, H::UserId>) {
        info!("Client disconnected. Cleaning up...");
        self.handler.on_disconnect(&context).await;

        self.state.connections.remove(&context.conn_id);

        let mut was_last_on_node = false;
        if let Some(mut subscribers) = self.state.subscriptions.get_mut(&context.topic) {
            subscribers.remove(&context.conn_id);
            if subscribers.is_empty() {
                was_last_on_node = true;
            }
        }

        if was_last_on_node {
            self.state.subscriptions.remove(&context.topic);
            info!(topic = %context.topic, "Last subscriber disconnected. Sending Unsubscribe command.");
            if let Err(e) = self
                .redis_cmd_tx
                .send(RedisCommand::Unsubscribe(context.topic))
                .await
            {
                error!("Failed to send Unsubscribe command to listener: {}", e);
            }
        }
    }

    /// Helper to send any serializable JSON value to a client's sink.
    async fn send_json_to_sink<T: Serialize>(&self, data: T, sink: &WsSink) -> Result<(), ()> {
        let json_string = serde_json::to_string(&data).map_err(|e| {
            error!("Failed to serialize response for client: {}", e);
        })?;
        sink.lock()
            .await
            .send(Message::Text(Utf8Bytes::from(json_string)))
            .await
            .map_err(|e| {
                warn!("Failed to send message to client sink: {}", e);
            })
    }

    /// Helper to send a standardized error message to a client.
    async fn send_error_response(&self, message: &str, sink: &WsSink) {
        let error_response = serde_json::json!({
            "type": "error",
            "message": message
        });
        if self.send_json_to_sink(error_response, sink).await.is_err() {
            warn!("Could not send error response. Client connection is likely dead.");
        }
    }
}
