// file: src/health.rs
// version: 1.1.0
// guid: 416df5cc-ca4d-4537-9e70-832f54a196f1
// last-edited: 2026-10-10

//! The gate an agent must pass before it may restart its node.

/// A cluster-wide snapshot taken just before an agent restarts its node.
#[derive(Debug, Clone, Default)]
pub struct ClusterHealth {
    /// Members that are not decommissioning or decommissioned.
    pub active_nodes: usize,
    /// Of `active_nodes`, how many gossip reports as live right now.
    pub live_nodes: usize,
    /// Sum of `ranges.underreplicated` over every store.
    pub underreplicated_ranges: i64,
    /// Sum of `ranges.unavailable` over every store.
    pub unavailable_ranges: i64,
}

/// Decides whether it is safe to take one more node down right now.
///
/// Returns `Ok(())` to proceed, or `Err(reason)` to wait. The reason is
/// recorded in `agent_status.error` and shown by `status`, so write it for an
/// operator reading it later.
///
/// Called only while holding the install lease, so at most one node is
/// ever restarting. The question is whether the cluster has fully recovered
/// from the *previous* restart.
pub fn safe_to_restart(health: &ClusterHealth) -> Result<(), String> {
    if health.active_nodes == 0 {
        return Err("no active cluster members observed; refusing to restart".to_string());
    }
    if health.live_nodes < health.active_nodes {
        return Err(format!(
            "{} of {} active nodes are not live; waiting for them to rejoin",
            health.active_nodes - health.live_nodes,
            health.active_nodes
        ));
    }
    if health.unavailable_ranges > 0 {
        return Err(format!(
            "{} ranges are unavailable; waiting for quorum to recover",
            health.unavailable_ranges
        ));
    }
    if health.underreplicated_ranges > 0 {
        return Err(format!(
            "{} ranges are under-replicated; waiting for up-replication",
            health.underreplicated_ranges
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> ClusterHealth {
        ClusterHealth {
            active_nodes: 5,
            live_nodes: 5,
            underreplicated_ranges: 0,
            unavailable_ranges: 0,
        }
    }

    #[test]
    fn fully_healthy_cluster_may_restart() {
        assert!(safe_to_restart(&healthy()).is_ok());
    }

    #[test]
    fn a_node_already_down_blocks_restart() {
        let health = ClusterHealth {
            live_nodes: 4,
            ..healthy()
        };
        assert!(safe_to_restart(&health).is_err());
    }

    #[test]
    fn underreplication_blocks_restart() {
        let health = ClusterHealth {
            underreplicated_ranges: 3,
            ..healthy()
        };
        assert!(safe_to_restart(&health).is_err());
    }

    #[test]
    fn unavailable_ranges_block_restart() {
        let health = ClusterHealth {
            unavailable_ranges: 1,
            ..healthy()
        };
        assert!(safe_to_restart(&health).is_err());
    }

    #[test]
    fn an_empty_snapshot_blocks_restart() {
        assert!(safe_to_restart(&ClusterHealth::default()).is_err());
    }
}
