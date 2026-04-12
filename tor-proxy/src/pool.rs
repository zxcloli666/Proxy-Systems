use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{error, info};

use crate::control::send_newnym;

/// Configuration for a single Tor node.
#[derive(Clone)]
pub struct TorNodeConfig {
    pub host: String,
    pub socks_port: u16,
    pub control_port: u16,
}

/// Runtime state for a single Tor node.
struct NodeState {
    config: TorNodeConfig,
    consecutive_errors: u32,
    last_newnym: Instant,
    cooldown_until: Instant,
    stats: NodeStats,
}

#[derive(Clone, serde::Serialize)]
pub struct NodeStats {
    pub requests: u64,
    pub success: u64,
    pub errors: u64,
    pub rotations: u64,
}

/// Public view of a node's status.
#[derive(serde::Serialize)]
pub struct NodeStatus {
    pub host: String,
    pub available: bool,
    #[serde(rename = "consecutiveErrors")]
    pub consecutive_errors: u32,
    pub stats: NodeStats,
}

/// Pool of Tor nodes with round-robin selection and automatic rotation.
pub struct NodePool {
    nodes: RwLock<Vec<NodeState>>,
    rr_idx: RwLock<usize>,
    password: String,
    error_threshold: u32,
    newnym_cooldown: Duration,
}

impl NodePool {
    pub fn new(
        configs: Vec<TorNodeConfig>,
        password: String,
        error_threshold: u32,
        newnym_cooldown: Duration,
        rotation_interval: Duration,
    ) -> Arc<Self> {
        let nodes: Vec<NodeState> = configs
            .into_iter()
            .map(|config| NodeState {
                config,
                consecutive_errors: 0,
                last_newnym: Instant::now() - Duration::from_secs(3600),
                cooldown_until: Instant::now() - Duration::from_secs(1),
                stats: NodeStats {
                    requests: 0,
                    success: 0,
                    errors: 0,
                    rotations: 0,
                },
            })
            .collect();

        let pool = Arc::new(Self {
            nodes: RwLock::new(nodes),
            rr_idx: RwLock::new(0),
            password,
            error_threshold,
            newnym_cooldown,
        });

        // Start scheduled rotation for each node (staggered)
        let node_count = {
            // We know the count from initialization
            pool.nodes.try_read().map(|n| n.len()).unwrap_or(1)
        };
        let stagger = rotation_interval / node_count as u32;

        for i in 0..node_count {
            let pool_clone = Arc::clone(&pool);
            let initial_delay = stagger * i as u32 + rotation_interval;
            tokio::spawn(async move {
                tokio::time::sleep(initial_delay).await;
                let mut interval = tokio::time::interval(rotation_interval);
                loop {
                    interval.tick().await;
                    info!("Scheduled rotation: node {}", i);
                    pool_clone.rotate_node(i).await;
                }
            });
        }

        pool
    }

    /// Get nodes ordered for round-robin, skipping nodes in cooldown.
    pub async fn get_ordered_nodes(&self) -> Vec<(usize, TorNodeConfig)> {
        let nodes = self.nodes.read().await;
        let now = Instant::now();

        let mut pool: Vec<(usize, &NodeState)> = nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.cooldown_until <= now)
            .collect();

        if pool.is_empty() {
            // All cooling down — use all
            pool = nodes.iter().enumerate().collect();
        }

        let mut rr = self.rr_idx.write().await;
        *rr = (*rr + 1) % pool.len();

        let ordered: Vec<(usize, TorNodeConfig)> = pool
            .iter()
            .cycle()
            .skip(*rr)
            .take(pool.len())
            .map(|(idx, n)| (*idx, n.config.clone()))
            .collect();

        ordered
    }

    /// Record a successful request for a node.
    pub async fn on_success(&self, node_idx: usize) {
        let mut nodes = self.nodes.write().await;
        if let Some(node) = nodes.get_mut(node_idx) {
            node.consecutive_errors = 0;
            node.stats.success += 1;
            node.stats.requests += 1;
        }
    }

    /// Record a failed request for a node. Triggers rotation if error threshold is reached.
    pub async fn on_error(&self, node_idx: usize) {
        let should_rotate = {
            let mut nodes = self.nodes.write().await;
            if let Some(node) = nodes.get_mut(node_idx) {
                node.consecutive_errors += 1;
                node.stats.errors += 1;
                node.stats.requests += 1;
                node.consecutive_errors >= self.error_threshold
            } else {
                false
            }
        };

        if should_rotate {
            info!(
                "Node {}: {} consecutive errors -> rotate",
                node_idx, self.error_threshold
            );
            self.rotate_node(node_idx).await;
        }
    }

    /// Send NEWNYM to rotate a node's Tor circuit.
    async fn rotate_node(&self, node_idx: usize) {
        let (host, control_port, can_rotate) = {
            let nodes = self.nodes.read().await;
            if let Some(node) = nodes.get(node_idx) {
                let elapsed = node.last_newnym.elapsed();
                (
                    node.config.host.clone(),
                    node.config.control_port,
                    elapsed >= self.newnym_cooldown,
                )
            } else {
                return;
            }
        };

        if !can_rotate {
            return;
        }

        match send_newnym(&host, control_port, &self.password).await {
            Ok(()) => {
                let mut nodes = self.nodes.write().await;
                if let Some(node) = nodes.get_mut(node_idx) {
                    node.last_newnym = Instant::now();
                    node.cooldown_until = Instant::now() + self.newnym_cooldown;
                    node.consecutive_errors = 0;
                    node.stats.rotations += 1;
                    info!("NEWNYM {} (rotation #{})", host, node.stats.rotations);
                }
            }
            Err(e) => {
                error!("NEWNYM {} failed: {}", host, e);
            }
        }
    }

    /// Get status for all nodes (for health endpoint).
    pub async fn status(&self) -> Vec<NodeStatus> {
        let nodes = self.nodes.read().await;
        let now = Instant::now();
        nodes
            .iter()
            .map(|n| NodeStatus {
                host: n.config.host.clone(),
                available: n.cooldown_until <= now,
                consecutive_errors: n.consecutive_errors,
                stats: n.stats.clone(),
            })
            .collect()
    }
}
