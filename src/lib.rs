//! # Axum Realtime Kit
//!
//! A toolkit for building scalable, real-time applications with Axum, WebSockets,
//! and Redis Pub/Sub. This crate provides generic building blocks to help you focus
//! on your application's business logic instead of the boilerplate for managing
//! connections and state.
//!
//! ## Core Features
//!
//! - **Generic `WebsocketService`**: Manages the entire lifecycle of WebSocket connections.
//! - **Redis Pub/Sub Integration**: Horizontally scale your service across multiple instances.
//! - **Pluggable Logic**: Use the `MessageHandler` trait to define your application's behavior.
//! - **Flexible Authentication**: A generic `WsAuth` extractor that works with headers or query params.
//! - **Request Coalescing (Optional)**: A generic `CoalescingService` to prevent dogpiling on expensive operations.
//!
//! ## Getting Started
//!
//! See the documentation for the `ws` module for a full example of how to set up the `WebsocketService`.
//!
//! ---

// The `ws` module contains all WebSocket-related logic.
pub mod ws;

// It will only be part of the crate if the "coalescing" feature is enabled.
#[cfg(feature = "coalescing")]
pub mod coalescing;

// It will only be part of the crate if the "auth" feature is enabled.
#[cfg(feature = "auth")]
pub mod auth;

/// Public prelude for convenience.
///
/// This allows users to import the most common types with a single `use` statement:
/// `use axum_realtime_kit::prelude::*;`
pub mod prelude {
    // Re-export the main WebSocket service components.
    pub use crate::ws::{
        handler::{ConnectionContext, MessageHandler},
        service::WebsocketService,
        upgrade::upgrade_handler,
    };

    // Re-export the CoalescingService if the feature is enabled.
    #[cfg(feature = "coalescing")]
    pub use crate::coalescing::CoalescingService;

    // Re-export the WsAuth extractor if the feature is enabled.
    #[cfg(feature = "auth")]
    pub use crate::auth::WsAuth;
}
