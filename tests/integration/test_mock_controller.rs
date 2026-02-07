//! Integration test for mock controller

use mock_controller::MockAtlasController;

#[tokio::test]
async fn test_mock_controller_startup() {
    // Start mock controller
    let controller = MockAtlasController::new().await.unwrap();

    // Verify ports are assigned
    assert!(controller.ssh_port() > 0);
    assert!(controller.http_port() > 0);

    println!("Mock controller running:");
    println!("  SSH port: {}", controller.ssh_port());
    println!("  HTTP port: {}", controller.http_port());
}

#[tokio::test]
#[ignore] // Requires full probe implementation
async fn test_measurement_flow() {
    // This will be implemented once we have the actual probe
    // For now, it serves as documentation of the intended API

    // let controller = MockAtlasController::new().await.unwrap();
    // let probe = AtlasProbe::new_test(controller.ssh_port()).await.unwrap();
    //
    // controller.send_measurement("300 0 0 UNIFORM 60 evping -c 3 8.8.8.8").await.unwrap();
    // let result = controller.wait_for_result(Duration::from_secs(10)).await.unwrap();
    //
    // assert_eq!(result.fw, 6000);
    // assert_eq!(result.measurement_type, MeasurementType::Ping);
}
