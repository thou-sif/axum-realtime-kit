# Axum Realtime Kit

[![Crates.io](https://img.shields.io/crates/v/axum-realtime-kit.svg)](https://crates.io/crates/axum-realtime-kit)
[![Docs.rs](https://docs.rs/axum-realtime-kit/badge.svg)](https://docs.rs/axum-realtime-kit)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](./LICENSE)
<!-- [![Build Status](https://github.com/your-username/axum-realtime-kit/actions/workflows/ci.yml/badge.svg)](https://github.com/your-username/axum-realtime-kit/actions) -->

A toolkit for building scalable, real-time applications with **Axum**, **WebSockets**, and **Redis Pub/Sub**. This crate provides the architectural building blocks to handle connection management, state synchronization, and authentication, letting you focus on your application's business logic.

## When to Use This Crate?

This crate is a great fit if you are building:

-   **Real-time Chat Applications:** Scale your chat rooms across multiple server instances without worrying about which user is connected to which server.
-   **Live Data Dashboards:** Push live metrics, logs, or financial data to thousands of connected clients simultaneously.
-   **Notifications Systems:** Broadcast real-time notifications to users subscribed to specific topics (e.g., "user:123", "project:456").
-   **Collaborative Applications:** Build tools like collaborative whiteboards or document editors where actions from one user must be reflected instantly for others.
-   **Multiplayer Game Lobbies:** Manage players in lobbies and broadcast game state changes.

## Core Concepts

The kit is designed around a few key abstractions:

1.  **`WebsocketService`**: The central engine. It manages all active connections on a server instance, communicates with Redis, and drives the entire lifecycle. You create one instance of this and share it across your Axum state.
2.  **`MessageHandler` Trait**: This is where **your application's logic lives**. You implement this trait to define your message and event types, handle incoming messages, and react to connections/disconnections.
3.  **`upgrade_handler` Function**: A generic Axum handler that orchestrates authentication and topic validation before upgrading an HTTP request to a WebSocket connection.

## Getting Started

The core pattern for using this crate involves four main steps.

### 1. Dependencies

Add `axum-realtime-kit` to your `Cargo.toml`.

```toml
[dependencies]
axum-realtime-kit = { version = "0.1.0", features = ["auth"] }
# ... other dependencies like tokio, serde, uuid ...
```

### 2. Define Your Models

Define the structs and enums that model your application's data flow, such as your `User` identity, the `ClientMessage`s you expect, and the `ServerEvent`s you will broadcast.

```rust
// Your authenticated user identity
struct User { /* ... */ }

// Messages clients send to the server
enum ClientMessage { /* ... */ }

// Events the server broadcasts to clients
enum ServerEvent { /* ... */ }
```

### 3. Implement the Core Traits

Plug your logic into the `WebsocketService` by implementing two traits on your application's state and a handler struct.

-   **`TokenValidator`**: Implement this on your shared Axum state to define how a string token from a request is validated and turned into your `User` struct.
-   **`MessageHandler`**: Implement this on a dedicated handler struct. This is where you'll write the logic for what happens when a client sends a message.

```rust
// In your state implementation
#[async_trait]
impl TokenValidator for AppState { /* ... auth logic ... */ }

// In your handler implementation
#[async_trait]
impl MessageHandler for ChatMessageHandler { /* ... message handling logic ... */ }
```

### 4. Wire It Up in Axum

In your `main` function, create the `WebsocketService`, wrap it in your Axum state, and create a route that uses the `upgrade_handler` to bring it all together.

> 📝 **For a complete, runnable implementation of these steps, please see the `basic_chat` example in the [`/examples`](./examples) directory.**

## Running the Example

You can run the included `basic_chat` example to see the kit in action.

1.  **Clone the repository:**
    ```sh
    git clone https://github.com/your-username/axum-realtime-kit.git
    cd axum-realtime-kit
    ```

2.  **Set up Redis:**
    Make sure you have a Redis server running. Create a `.env` file in the project root and add your Redis connection URL:
    ```
    # .env
    REDIS_URL="redis://127.0.0.1/"
    ```

3.  **Run the server:**
    ```sh
    cargo run --example basic_chat
    ```
    The server will start listening on `127.0.0.1:3000`.

4.  **Connect with WebSocket clients:**
    Open two separate terminals and use a tool like `websocat`.

    -   **Terminal 1 (Alice):** Connect to the `general` room.
        ```sh
        websocat "ws://127.0.0.1:3000/ws/general?token=a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8:alice"
        ```

    -   **Terminal 2 (Bob):** Connect to the same room and send a message.
        ```sh
        # Connect
        websocat "ws://127.0.0.1:3000/ws/general?token=b1b2b3b4-a1a2-b1b2-c1c2-c3c4c5c6c7c8:bob"
        
        # Type this and press Enter to send a message
        > {"SendMessage":{"room":"general","text":"Hello from Bob!"}}
        ```

You will see Bob's message broadcast and appear in Alice's terminal.

## Feature Flags

-   `auth` (default): Enables the `WsAuth` extractor and `TokenValidator` trait for authenticating WebSocket upgrade requests.
-   `coalescing`: Enables the `CoalescingService`, a utility for preventing duplicate concurrent executions of an expensive asynchronous operation.

## License

This project is licensed under either of

-   Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
-   MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.