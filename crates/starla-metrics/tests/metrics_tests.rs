//! Integration tests for starla-metrics
//!
//! These tests verify the metrics implementation.

#[cfg(feature = "export")]
mod with_export {
    use starla_metrics::{server::start_metrics_server, MetricsRegistry};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn test_metrics_registry_creation() {
        let registry = MetricsRegistry::new().expect("Failed to create registry");

        // Record some initial metrics to ensure they appear in gather()
        registry.record_measurement_started("ping");
        registry.update_pending_count(0);

        let metrics = registry.gather();
        let names: Vec<_> = metrics.iter().map(|m| m.get_name()).collect();
        assert!(names.contains(&"starla_measurements_total"));
        assert!(names.contains(&"starla_measurements_pending"));
    }

    #[test]
    fn test_measurement_recording() {
        let registry = MetricsRegistry::new().unwrap();
        registry.record_measurement_completed("ping", 0.1);
        registry.record_measurement_failed("traceroute", 0.5);

        let metrics = registry.gather();
        let total = metrics
            .iter()
            .find(|m| m.get_name() == "starla_measurements_total")
            .unwrap();
        // ping completed and traceroute failed = at least 2 different label
        // combinations
        assert!(total.get_metric().len() >= 2);
    }

    #[test]
    fn test_gauge_updates() {
        let registry = MetricsRegistry::new().unwrap();
        registry.update_pending_count(10);
        registry.update_database_size(1024);

        let metrics = registry.gather();
        let pending = metrics
            .iter()
            .find(|m| m.get_name() == "starla_measurements_pending")
            .unwrap();
        assert_eq!(pending.get_metric()[0].get_gauge().get_value(), 10.0);
    }

    #[tokio::test]
    async fn test_metrics_server() {
        let registry = Arc::new(MetricsRegistry::new().unwrap());
        let addr = "127.0.0.1:0".parse().unwrap();
        let cancel_token = CancellationToken::new();
        let cancel_clone = cancel_token.clone();

        let server_handle =
            tokio::spawn(async move { start_metrics_server(registry, addr, cancel_clone).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_token.cancel();

        let result = tokio::time::timeout(Duration::from_secs(1), server_handle).await;
        assert!(result.is_ok());
    }
}

#[cfg(not(feature = "export"))]
mod without_export {
    use starla_metrics::MetricsRegistry;

    #[test]
    fn test_no_op_registry() {
        let registry = MetricsRegistry::new().expect("Failed to create registry");
        registry.record_measurement_completed("ping", 0.1);
        registry.update_pending_count(5);
        registry.record_upload_success();
        registry.record_cleanup_run(1, 2, 3);
        registry.record_cleanup_duration(0.1);
        // Should not panic and do nothing
    }
}
