use prometheus::{IntCounter, IntGauge};

pub const PROMETHEUS_HTTP_PORT: u16 = 9091;

#[derive(Clone)]
pub struct NodeManagerMetrics {
    pub heart_beat_count: IntCounter,
    pub resident_mem_used: IntGauge,
    /// Registry version last used to succesfully fetch datacenter information
    pub datacenter_registry_version: IntGauge,
}

impl NodeManagerMetrics {
    pub fn new(metrics_registry: &ic_metrics::MetricsRegistry) -> Self {
        Self {
            heart_beat_count: metrics_registry.int_counter(
                "replica_heart_beat_count",
                "Number of times a process heart beat has been observed for the Subnet Replica",
            ),
            resident_mem_used: metrics_registry.int_gauge(
                "replica_resident_memory_used",
                "Resident memory allocated by the Subnet Replica in bytes",
            ),
            datacenter_registry_version: metrics_registry.int_gauge(
                "datacenter_registry_version",
                "Registry version last used to successfully fetch datacenter information",
            ),
        }
    }
}
