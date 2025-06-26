use axum::{
    extract::{Path as AxumPath, State, WebSocketUpgrade}, // Renamed Path to avoid conflict
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use axum_realtime_kit::{
    auth::{TokenValidator, WsAuth}, // WsAuth needs the "auth" feature
    prelude::*, // Includes MessageHandler, WebsocketService, upgrade_handler, ConnectionContext
    ws::handler::HandlerError,
};
use serde::{Deserialize, Serialize};
use std::{fmt, net::SocketAddr, sync::Arc};
use tracing::info;
use uuid::Uuid;
use std::error::Error as StdError;

// 1. Define User, Messages, Events, and AppState

#[derive(Debug)]
struct AuthError(String);

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl StdError for AuthError {}

#[derive(Debug, Clone)]
struct User {
    id: Uuid,
    username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ClientMessage {
    SendMessage { room: String, text: String },
    Ping, // A message that expects a direct response
}

#[derive(Debug, Clone, Serialize)]
enum ServerEvent {
    UserJoined { username: String, room: String },
    NewMessage { room: String, username: String, text: String },
    Pong, // Direct response content
}

#[derive(Clone)]
struct AppState {
    // In a real app, this might hold a DB connection pool, config, etc.
    server_secret: String,
}

// 2. Implement TokenValidator for your AppState (or a combined state)

#[async_trait::async_trait]
impl TokenValidator for AppState {
    type User = User;
    type Error = AuthError;

    async fn validate_token(&self, token: &str) -> Result<Self::User, Self::Error> {
        info!("Validating token: {}", token);
        info!("Raw token: {:?}", token);

        // First, without expecting a Bearer prefix
        let parts_str = if token.starts_with("Bearer ") {
            info!("Token has Bearer prefix, stripping it");
            token.strip_prefix("Bearer ").unwrap()
        } else {
            info!("Token doesn't have Bearer prefix, using as is");
            token
        };

        info!("Processing token part: {:?}", parts_str);

        let parts: Vec<&str> = parts_str.splitn(2, ':').collect();
        info!("Split parts: {:?}", parts);

        if parts.len() == 2 {
            if let Ok(id) = Uuid::parse_str(parts[0]) {
                let username = parts[1].to_string();
                info!("Token validated for user: {} ({})", username, id);
                return Ok(User { id, username });
            } else {
                info!("Failed to parse UUID: {}", parts[0]);
            }
        } else {
            info!("Invalid number of parts: {}", parts.len());
        }

        Err(AuthError("Invalid token format or content".to_string()))
    }
}

// 3. Implement MessageHandler

struct ChatMessageHandler;

#[async_trait::async_trait]
impl MessageHandler for ChatMessageHandler {
    type ClientMessage = ClientMessage;
    type ServerEvent = ServerEvent;
    type AppState = AppState; // The app state used by WebsocketService
    type UserId = Uuid; // The type of User::id

    async fn on_connect(
        &self,
        context: &ConnectionContext<Self::AppState, Self::UserId>,
    ) {
        info!(
            "User {} ({}) connected to topic '{}'",
            context.user_id, // This would be more useful if we had the full User struct here
                              // but WsAuth only passes the ID to upgrade_handler.
                              // We can fetch user details in on_connect if needed from AppState.
            context.user_id, // placeholder for username, see comment above
            context.topic
        );
        // Example: Announce user joined. (Requires user details)
        // For simplicity, we'll skip fetching full user details here.
        // A real app might:
        // let user_details = context.app_state.get_user_by_id(context.user_id).await;
        // self.publish_event(&context.topic, ServerEvent::UserJoined { username: user_details.username, room: context.topic.clone() }, context).await;
    }

    async fn handle_direct_message(
        &self,
        msg: &Self::ClientMessage,
        _context: &ConnectionContext<Self::AppState, Self::UserId>,
    ) -> Result<Option<serde_json::Value>, HandlerError> {
        match msg {
            ClientMessage::Ping => {
                info!("Received Ping, sending Pong directly");
                // Ok(Some(ServerEvent::Pong)) // This would work if ServerEvent was directly serializable to Value
                Ok(Some(serde_json::to_value(ServerEvent::Pong).map_err(HandlerError::Serialization)?))
            }
            _ => Ok(None), // Fall through to broadcast handler
        }
    }

    async fn handle_broadcast_message(
        &self,
        msg: Self::ClientMessage,
        context: &ConnectionContext<Self::AppState, Self::UserId>,
    ) -> Result<Option<Self::ServerEvent>, HandlerError> {
        match msg {
            ClientMessage::SendMessage { room, text } => {
                // Here, you'd typically fetch the username associated with context.user_id
                // For simplicity, we'll just use the ID as a placeholder.
                // In a real app: let username = get_username_from_db(context.user_id, &context.app_state).await;
                let username = format!("User_{}", context.user_id.to_string().split('-').next().unwrap_or("anon"));
                info!(
                    "User {} broadcasting message to room '{}': {}",
                    username, room, text
                );

                // Basic validation: ensure the message is for the topic the user is subscribed to.
                // The `upgrade_handler`'s validator already confirms general topic access.
                if room != context.topic {
                    return Err(HandlerError::Custom(
                        StatusCode::BAD_REQUEST,
                        Some(format!("Cannot send to room '{}' from topic '{}'", room, context.topic))
                    ));
                }

                Ok(Some(ServerEvent::NewMessage {
                    room: room.clone(),
                    username,
                    text: text.clone(),
                }))
            }
            ClientMessage::Ping => {
                // This case should ideally be handled by handle_direct_message
                // If it reaches here, it means handle_direct_message returned Ok(None)
                info!("Ping reached broadcast_message handler (should not happen if direct_message handles it)");
                Ok(None)
            }
        }
    }

    async fn on_disconnect(
        &self,
        context: &ConnectionContext<Self::AppState, Self::UserId>,
    ) {
        info!("User {} disconnected from topic '{}'", context.user_id, context.topic);
        // Example: Announce user left
        // self.publish_event(&context.topic, ServerEvent::UserLeft { username: context.user_id.to_string(), room: context.topic.clone() }, context).await;
    }
}

// 4. Topic Validation Logic (for the upgrade_handler)
async fn validate_topic_access(
    _app_state: Arc<AppState>, // Can be used to check against DB, etc.
    user_id: Uuid,
    topic: String,
) -> Result<(), StatusCode> {
    info!(
        "Validating topic access for user {} to topic '{}'",
        user_id, topic
    );
    // Example: Allow any topic that doesn't start with "private-"
    // Or if it's "private-user-<id>", only allow that user.
    if topic.starts_with("private-") {
        let expected_topic = format!("private-user-{}", user_id);
        if topic == expected_topic {
            return Ok(());
        }
        info!("Access to {} forbidden for user {}", topic, user_id);
        return Err(StatusCode::FORBIDDEN);
    }
    // Allow access to other "public" topics
    if topic == "general" || topic == "random" {
        return Ok(());
    }
    Err(StatusCode::NOT_FOUND) // Or FORBIDDEN if the topic format is known but not allowed
}

// 5. Axum State combining AppState and WebsocketService
// The Axum State needs to implement TokenValidator if WsAuth is used at the same router level.
#[derive(Clone)]
struct ServerState {
    app_state: Arc<AppState>,
    ws_service: Arc<WebsocketService<ChatMessageHandler>>,
}

#[async_trait::async_trait]
impl TokenValidator for ServerState {
    type User = User;
    type Error = AuthError;

    async fn validate_token(&self, token: &str) -> Result<Self::User, Self::Error> {
        // Delegate to the AppState's validator
        self.app_state.validate_token(token).await
    }
}

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("basic_chat=info".parse().unwrap()).add_directive("axum_realtime_kit=info".parse().unwrap()))
        .init();

    let app_specific_state = Arc::new(AppState {
        server_secret: "super-secret-key".to_string(),
    });

    let chat_handler = ChatMessageHandler;

    let redis_url = std::env::var("REDIS_URL")
        .unwrap_or_else(|_| {
            info!("REDIS_URL not found in environment, using default");
            "redis://default:jZCdns.redis-cloud.com:13656".to_string()
        });

    let ws_service = WebsocketService::new(&redis_url, chat_handler, Arc::clone(&app_specific_state));

    let server_state = ServerState {
        app_state: app_specific_state,
        ws_service,
    };

    let app = Router::new()
        .route(
            "/ws/{topic}",
            get(
                |ws: WebSocketUpgrade,
                 State(state): State<ServerState>,
                 AxumPath(topic): AxumPath<String>, // topic from path
                 WsAuth(user): WsAuth<User>, // Authenticated user from WsAuth
                | async move {
                    let user_id = user.id; // Get the UserId
                    // The WebsocketService needs H::AppState, not the combined ServerState
                    let app_state_for_validator = Arc::clone(&state.app_state);
                    upgrade_handler(
                        ws,
                        State(state.ws_service), // Pass the WebsocketService itself
                        user_id,
                        topic,
                        // Provide the validation closure
                        |app_s, u_id, t| validate_topic_access(app_s, u_id, t),
                    )
                        .await
                },
            ),
        )
        .route("/health", get(|| async { "OK" }))
        .with_state(server_state);

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    info!("Listening on {}", addr);
    info!("You can connect using wscat or websocat.");

    axum::serve(tokio::net::TcpListener::bind(addr).await.unwrap(), app)
        .await
        .unwrap();
}
