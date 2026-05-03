//! Integration tests for starla-metrics

#[cfg(feature = "export")]
mod with_export {
    use starla_metrics::{server::start_metrics_server, MetricsRegistry};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn test_metrics_registry_creation() {
        let registry = MetricsRegistry::new().expect("Failed to create registry");

        registry.record_measurement_started("ping");
        registry.update_queue_depth(0);

        let metrics = registry.gather();
        let names: Vec<_> = metrics.iter().map(|m| m.get_name()).collect();
        assert!(names.contains(&"starla_measurements_started_total"));
        assert!(names.contains(&"starla_upload_queue_depth"));
    }

    #[test]
    fn test_measurement_recording() {
        let registry = MetricsRegistry::new().unwrap();
        registry.record_measurement_completed("ping", 0.1);
        registry.record_measurement_failed("traceroute", 0.5);

        let metrics = registry.gather();
        let completed = metrics
            .iter()
            .find(|m| m.get_name() == "starla_measurements_completed_total")
            .unwrap();
        assert!(!completed.get_metric().is_empty());
    }

    #[test]
    fn test_gauge_updates() {
        let registry = MetricsRegistry::new().unwrap();
        registry.update_queue_depth(10);
        registry.set_connected(true);

        let metrics = registry.gather();
        let queue = metrics
            .iter()
            .find(|m| m.get_name() == "starla_upload_queue_depth")
            .unwrap();
        assert_eq!(queue.get_metric()[0].get_gauge().get_value(), 10.0);

        let conn = metrics
            .iter()
            .find(|m| m.get_name() == "starla_controller_connected")
            .unwrap();
        assert_eq!(conn.get_metric()[0].get_gauge().get_value(), 1.0);
    }

    #[tokio::test]
    async fn test_metrics_server() {
        let registry = MetricsRegistry::new().unwrap();
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
        registry.record_upload_success();
        registry.set_connected(true);
        registry.record_queue_drop();
    }
}
