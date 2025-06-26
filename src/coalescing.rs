// src/coalescing.rs
use anyhow::{Error, Result, anyhow};
use futures::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use std::{collections::HashMap, fmt::Debug, hash::Hash, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{Span, debug, error, info, info_span, instrument, warn}; // Added tracing imports

// --- Type Aliases ---

/// A cloneable, shareable future returning the operation's result wrapped in Arcs.
/// The outer Arc<...> allows sharing the Result itself.
/// The inner Result<Arc<T>, Arc<Error>> allows sharing the success value (T)
/// or the error value (Error) cheaply via Arcs.
type SharedOp<T> = Shared<BoxFuture<'static, Arc<Result<Arc<T>, Arc<Error>>>>>;

// --- Statistics ---

/// Statistics for monitoring the coalescing service.
#[derive(Debug, Default, Clone)]
pub struct CoalescingStats {
    /// Number of operations initiated (first request for a given key).
    pub initiated_operations: usize,
    /// Number of requests that were coalesced (joined an existing operation).
    pub coalesced_requests: usize,
    /// Number of initiated operations that resulted in failure (excluding timeouts).
    pub failed_operations: usize,
    /// Number of initiated operations that timed out.
    pub timed_out_operations: usize,
}

// --- Service Implementation ---

/// A service that coalesces identical asynchronous operations based on a key
/// to reduce load on the underlying resource (e.g., database, external API).
#[derive(Debug)]
pub struct CoalescingService<K, T>
where
    K: Eq + Hash + Clone + Debug + Send + Sync + 'static,
    T: Send + Sync + 'static, // T no longer needs Clone here
{
    /// Stores the currently running shared operations.
    inner: Arc<Mutex<HashMap<K, SharedOp<T>>>>,
    /// Optional global timeout for operations initiated by this service.
    timeout: Option<Duration>,
    /// Shared statistics.
    stats: Arc<Mutex<CoalescingStats>>,
}

impl<K, T> CoalescingService<K, T>
where
    K: Eq + Hash + Clone + Debug + Send + Sync + 'static,
    T: Send + Sync + 'static,
{
    /// Create a new CoalescingService without a global timeout.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            timeout: None,
            stats: Arc::new(Mutex::new(CoalescingStats::default())),
        }
    }

    /// Create a new CoalescingService with a global operation timeout.
    /// Operations taking longer than this duration will fail with a timeout error.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            timeout: Some(timeout),
            stats: Arc::new(Mutex::new(CoalescingStats::default())),
        }
    }

    /// Execute an asynchronous operation associated with `key`.
    ///
    /// If another request for the same `key` is already in progress, this call
    /// will wait for the result of that ongoing operation instead of starting a new one.
    ///
    /// # Arguments
    /// * `key` - The key identifying the operation.
    /// * `operation` - An `FnOnce` closure that returns a Future resolving to `Result<T, anyhow::Error>`.
    ///                 This is only called if no operation for `key` is currently in progress.
    ///
    /// # Returns
    /// A `Result` containing either `Arc<T>` on success or `Arc<anyhow::Error>` on failure.
    /// The `Arc` allows cheap cloning of the result/error across multiple awaiters.
    #[instrument(skip(self, operation), fields(key = ?key))]
    pub async fn execute<F, Fut>(&self, key: K, operation: F) -> Result<Arc<T>, Arc<Error>>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T, Error>> + Send + 'static,
    {
        let mut maybe_newly_created = false; // Track if we inserted the future

        // --- 1. Check for existing operation (Lock Scope 1) ---
        let shared_op = {
            let mut map_guard = self.inner.lock().await;
            if let Some(existing_op) = map_guard.get(&key) {
                // --- 1a. Coalesce ---
                debug!("Request coalesced, joining existing operation.");
                let mut stats_guard = self.stats.lock().await;
                stats_guard.coalesced_requests += 1;
                existing_op.clone()
            } else {
                // --- 1b. Initiate New Operation ---
                info!("Initiating new operation.");
                maybe_newly_created = true;
                let mut stats_guard = self.stats.lock().await;
                stats_guard.initiated_operations += 1;
                // Drop stats lock quickly
                drop(stats_guard);

                let map_clone = Arc::clone(&self.inner);
                let stats_clone = Arc::clone(&self.stats);
                let operation_timeout = self.timeout;
                let key_clone_for_task = key.clone(); // Clone key for the task

                // Build the shared future. This async block runs *only once*.
                let new_shared_op = async move {
                    let task_span = info_span!("coalesced_op_worker", key = ?key_clone_for_task);
                    let _enter = task_span.enter(); // Enter span for task duration

                    info!("Executing underlying operation.");

                    // --- Step 1: Execute the operation and handle timeout ---
                    // Store the result locally to ensure cleanup always runs.
                    let op_result: Result<T, Error> = if let Some(dur) = operation_timeout {
                        match timeout(dur, operation()).await {
                            Ok(res) => res, // Result from operation()
                            Err(_) => {
                                // Timeout occurred
                                warn!("Operation timed out after {:?}", dur);
                                let mut stats_guard = stats_clone.lock().await;
                                stats_guard.timed_out_operations += 1;
                                // Create the timeout error, but don't return yet
                                Err(anyhow!(
                                    "Operation timed out for key: {:?}",
                                    key_clone_for_task
                                ))
                            }
                        }
                    } else {
                        // No timeout configured
                        operation().await
                    };

                    // --- Step 2: Cleanup: Remove self from the map ---
                    // This now runs *after* the operation completes or times out.
                    debug!("Operation finished or timed out, removing from active map.");
                    map_clone.lock().await.remove(&key_clone_for_task); // Always runs

                    // --- Step 3: Map the result and update stats ---
                    // Map Result<T, Error> to Result<Arc<T>, Arc<Error>>
                    // and record failures (excluding timeouts, which were handled above)
                    let final_result: Result<Arc<T>, Arc<Error>> = match op_result {
                        Ok(value) => Ok(Arc::new(value)),
                        Err(e) => {
                            // Check if it's the specific timeout error we created.
                            // If not, it's a regular failure from the operation itself.
                            // Use string comparison cautiously, ideally use a custom error type.
                            if !e.to_string().starts_with("Operation timed out for key") {
                                error!("Underlying operation failed: {:?}", e);
                                let mut stats_guard = stats_clone.lock().await;
                                stats_guard.failed_operations += 1;
                            }
                            Err(Arc::new(e)) // Wrap error in Arc anyway
                        }
                    };

                    // Return the final result wrapped in Arc for the Shared future state
                    Arc::new(final_result)
                }
                .boxed()
                .shared();

                // Insert the new SharedOp into the map *before* releasing the lock
                map_guard.insert(key.clone(), new_shared_op.clone());
                new_shared_op
            }
        };
        // --- Lock Scope 1 End --- (map_guard is dropped)

        if maybe_newly_created {
            debug!("Spawned and waiting for new operation future.");
        } else {
            debug!("Waiting for existing operation future.");
        }

        // --- 2. Await the Shared Future (Outside Lock) ---
        let result_arc: Arc<Result<Arc<T>, Arc<Error>>> = shared_op.await;

        // --- 3. Return the Cloned Result/Error Arc ---
        // Clone the Arc<T> or Arc<Error> from the shared result.
        // This is cheap and avoids cloning T or the Error itself.
        match &*result_arc {
            Ok(arc_t) => Ok(Arc::clone(arc_t)),
            Err(arc_e) => Err(Arc::clone(arc_e)),
        }
    }

    /// Get a snapshot of the current statistics.
    #[instrument(skip(self))]
    pub async fn get_stats(&self) -> CoalescingStats {
        self.stats.lock().await.clone()
    }

    /// Get the number of distinct keys whose operations are currently in flight.
    #[instrument(skip(self))]
    pub async fn get_pending_tasks_count(&self) -> usize {
        self.inner.lock().await.len()
    }

    /// Reset all statistics to zero.
    #[instrument(skip(self))]
    pub async fn reset_stats(&self) {
        info!("Resetting statistics.");
        let mut stats = self.stats.lock().await;
        *stats = CoalescingStats::default();
    }
}

// --- Clone & Default Implementations ---

impl<K, T> Clone for CoalescingService<K, T>
where
    K: Eq + Hash + Clone + Debug + Send + Sync + 'static,
    T: Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            timeout: self.timeout,
            stats: Arc::clone(&self.stats),
        }
    }
}

impl<K, T> Default for CoalescingService<K, T>
where
    K: Eq + Hash + Clone + Debug + Send + Sync + 'static,
    T: Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

// --- Example Usage (Requires tokio runtime) ---

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::sleep;
    use tracing_subscriber; // Make sure tracing-subscriber is in dev-dependencies

    // Ensure tracing subscriber is initialized only once for tests
    static TRACING: Lazy<()> = Lazy::new(|| {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG) // Adjust level as needed
            .with_test_writer() // Write to test output
            .init();
    });

    // Simulate a database or external call
    async fn simulate_fetch(key: &str, call_count: Arc<AtomicUsize>) -> Result<String, Error> {
        call_count.fetch_add(1, Ordering::SeqCst);
        info!(key = %key, "--- Executing simulate_fetch ---");
        sleep(Duration::from_millis(50)).await; // Simulate work
        if key == "fail_key" {
            Err(anyhow!("Failed to fetch {}", key))
        } else {
            Ok(format!("Data for {}", key))
        }
    }

    #[tokio::test]
    async fn test_coalescing() {
        Lazy::force(&TRACING); // Initialize tracing
        let service: CoalescingService<String, String> = CoalescingService::new();
        let call_count = Arc::new(AtomicUsize::new(0));

        let key = "test_key".to_string();

        // Spawn multiple concurrent requests for the same key
        let mut handles = vec![];
        for _ in 0..5 {
            let svc = service.clone();
            let k = key.clone();
            let cc = call_count.clone();
            handles.push(tokio::spawn(async move {
                svc.execute(k, move || simulate_fetch("test_key", cc)).await
            }));
        }

        // Wait for all requests to complete
        let mut results = vec![];
        for handle in handles {
            results.push(handle.await.unwrap());
        }

        // Verify results
        let expected_result = Arc::new("Data for test_key".to_string());
        for result in results {
            match result {
                Ok(data_arc) => assert_eq!(data_arc, expected_result),
                Err(e) => panic!("Expected success, got error: {:?}", e),
            }
        }

        // Verify call count (should only be 1)
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "simulate_fetch should only be called once"
        );

        // Verify stats
        let stats = service.get_stats().await;
        assert_eq!(stats.initiated_operations, 1);
        assert_eq!(stats.coalesced_requests, 4); // 5 total requests - 1 initiated = 4 coalesced
        assert_eq!(stats.failed_operations, 0);
        assert_eq!(stats.timed_out_operations, 0);

        assert_eq!(
            service.get_pending_tasks_count().await,
            0,
            "Map should be empty after completion"
        );
    }

    #[tokio::test]
    async fn test_failure() {
        Lazy::force(&TRACING); // Initialize tracing
        let service: CoalescingService<String, String> = CoalescingService::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let key = "fail_key".to_string();

        // Spawn multiple concurrent requests for the failing key
        let mut handles = vec![];
        for i in 0..3 {
            let svc = service.clone();
            let k = key.clone();
            let cc = call_count.clone();
            let task_id = i; // Identify task for debugging if needed
            handles.push(tokio::spawn(async move {
                debug!(task_id, "Task starting execute call");
                let res = svc.execute(k, move || simulate_fetch("fail_key", cc)).await;
                debug!(task_id, "Task finished execute call");
                res
            }));
        }

        // Wait for all requests to complete
        let mut results = vec![];
        for handle in handles {
            results.push(handle.await.unwrap());
        }

        // Verify results (all should be errors)
        for result in results {
            match result {
                Ok(_) => panic!("Expected failure, got success"),
                Err(e_arc) => {
                    assert!(
                        e_arc.to_string().contains("Failed to fetch fail_key"),
                        "Unexpected error message: {}",
                        e_arc
                    );
                }
            }
        }

        // Verify call count (should only be 1)
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "simulate_fetch should still only be called once"
        );

        // Verify stats
        let stats = service.get_stats().await;
        assert_eq!(stats.initiated_operations, 1);
        assert_eq!(stats.coalesced_requests, 2); // 3 total - 1 initiated = 2 coalesced
        assert_eq!(stats.failed_operations, 1); // The single initiated op failed
        assert_eq!(stats.timed_out_operations, 0);

        assert_eq!(
            service.get_pending_tasks_count().await,
            0,
            "Map should be empty after completion"
        );
    }

    #[tokio::test]
    async fn test_timeout() {
        Lazy::force(&TRACING); // Initialize tracing
        // Service with a short timeout
        let service: CoalescingService<String, String> =
            CoalescingService::with_timeout(Duration::from_millis(20));
        // Intentionally removed call_count as the operation won't finish anyway
        let key = "timeout_key".to_string();

        // Operation that takes longer than the timeout
        // Note: We don't need Arc<AtomicUsize> here as it won't complete.
        let long_op = || async {
            let key_in_op = "timeout_key"; // Can capture or just redefine
            info!(key = %key_in_op, "--- Executing long_op (will time out) ---");
            sleep(Duration::from_millis(100)).await;
            // This part is never reached due to timeout
            info!(key = %key_in_op, "--- Long op finished (should not see this) ---");
            Ok::<String, anyhow::Error>("Should not reach here".to_string())
        };

        // Spawn multiple concurrent requests
        let mut handles = vec![];
        for i in 0..3 {
            let svc = service.clone();
            let k = key.clone();
            let task_id = i;
            // Need to create a new closure instance for each spawn due to FnOnce
            // If long_op captured state that needed sharing, Arc or different approach needed.
            let op_fn = || async {
                sleep(Duration::from_millis(100)).await;
                Ok("Should not reach here".to_string())
            };

            handles.push(tokio::spawn(async move {
                debug!(task_id, "Task starting execute call for timeout key");
                let res = svc.execute(k, op_fn).await;
                debug!(task_id, "Task finished execute call for timeout key");
                res
            }));
        }

        // Wait for results
        let mut results = vec![];
        for handle in handles {
            results.push(handle.await.unwrap());
        }

        // Verify results (all should be timeout errors)
        for result in results {
            match result {
                Ok(_) => panic!("Expected timeout error, got success"),
                Err(e_arc) => {
                    assert!(
                        e_arc.to_string().contains("Operation timed out"),
                        "Unexpected error message: {}",
                        e_arc
                    );
                    assert!(
                        e_arc.to_string().contains("timeout_key"),
                        "Error message should contain the key: {}",
                        e_arc
                    );
                }
            }
        }

        // Verify stats
        let stats = service.get_stats().await;
        assert_eq!(stats.initiated_operations, 1);
        assert_eq!(stats.coalesced_requests, 2); // 3 total - 1 initiated
        assert_eq!(stats.failed_operations, 0); // Timeout is tracked separately
        assert_eq!(stats.timed_out_operations, 1); // The single initiated op timed out

        // --- THIS IS THE CRITICAL ASSERTION THAT FAILED BEFORE ---
        assert_eq!(
            service.get_pending_tasks_count().await,
            0,
            "Map should be empty after completion (even on timeout)"
        );
    }

    #[tokio::test]
    async fn test_independent_keys() {
        Lazy::force(&TRACING); // Initialize tracing
        let service: CoalescingService<String, String> = CoalescingService::new();
        let call_count = Arc::new(AtomicUsize::new(0));

        let key1 = "key1".to_string();
        let key2 = "key2".to_string();

        let cc1 = call_count.clone();
        let cc2 = call_count.clone();

        let service1 = service.clone();
        let service2 = service.clone();
        let key1_clone = key1.clone(); // Clone keys for moving into tasks
        let key2_clone = key2.clone();

        let h1 = tokio::spawn(async move {
            service1
                .execute(key1_clone, move || {
                    // Clone again inside the closure for potential retries if needed
                    // or just capture by move if FnOnce guarantees single call
                    let k1_for_op = key1.clone();
                    let c1 = cc1.clone();
                    async move { simulate_fetch(&k1_for_op, c1).await }
                })
                .await
        });

        let h2 = tokio::spawn(async move {
            service2
                .execute(key2_clone, move || {
                    let k2_for_op = key2.clone();
                    let c2 = cc2.clone();
                    async move { simulate_fetch(&k2_for_op, c2).await }
                })
                .await
        });

        let res1 = h1.await.unwrap();
        let res2 = h2.await.unwrap();

        assert_eq!(res1.unwrap(), Arc::new("Data for key1".to_string()));
        assert_eq!(res2.unwrap(), Arc::new("Data for key2".to_string()));

        assert_eq!(
            call_count.load(Ordering::SeqCst),
            2,
            "simulate_fetch should be called twice for independent keys"
        );

        let stats = service.get_stats().await;
        assert_eq!(stats.initiated_operations, 2);
        assert_eq!(stats.coalesced_requests, 0);
        assert_eq!(stats.failed_operations, 0);
        assert_eq!(stats.timed_out_operations, 0);

        assert_eq!(
            service.get_pending_tasks_count().await,
            0,
            "Map should be empty after completion"
        );
    }
}
