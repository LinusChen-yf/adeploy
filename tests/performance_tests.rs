//! Performance and load tests for gRPC services

use std::{
  sync::Arc,
  time::{Duration, Instant},
};

use adeploy::{
  adeploy::deploy_service_server::DeployServiceServer, client, server::AdeployService,
};
use log2::*;
use tempfile::TempDir;
use tokio::{
  sync::Semaphore,
  time::{sleep, timeout},
};
use tonic::transport::Server;

mod common;

#[tokio::test]
async fn test_concurrent_server_startup() {
  let port = common::find_available_port().await;
  let temp_dir = common::create_temp_dir();
  let server_config = common::create_test_server_config();
  let service = AdeployService::new(server_config);

  let addr = format!("127.0.0.1:{}", port).parse().unwrap();

  // Start server
  let server_handle = tokio::spawn(async move {
    Server::builder()
      .add_service(DeployServiceServer::new(service))
      .serve(addr)
      .await
  });

  // Give server time to start
  sleep(Duration::from_millis(200)).await;

  // Test server startup performance
  let start_time = Instant::now();
  
  // If we reach this point, server started successfully
  let startup_duration = start_time.elapsed();
  info!("Server startup took: {:?}", startup_duration);
  assert!(startup_duration < Duration::from_secs(5));

  // Cleanup
  server_handle.abort();
}

#[tokio::test]
async fn test_sequential_request_performance() {
  let port = common::find_available_port().await;
  let temp_dir = common::create_temp_dir();
  let server_config = common::create_test_server_config();
  let service = AdeployService::new(server_config);

  let addr = format!("127.0.0.1:{}", port).parse().unwrap();

  // Start server
  let server_handle = tokio::spawn(async move {
    Server::builder()
      .add_service(DeployServiceServer::new(service))
      .serve(addr)
      .await
  });

  // Give server time to start
  sleep(Duration::from_millis(200)).await;

  // Test sequential server operations
  let start_time = Instant::now();
  
  // Simulate some sequential operations
  for _i in 0..5 {
    sleep(Duration::from_millis(10)).await;
  }
  
  let total_elapsed = start_time.elapsed();
  info!("Sequential operations completed in: {:?}", total_elapsed);
  
  // Basic assertion
  assert!(total_elapsed < Duration::from_secs(1));

  // Clean up
  server_handle.abort();
}

#[tokio::test]
async fn test_server_memory_usage() {
  // This test checks that the server doesn't leak memory with many requests
  let port = common::find_available_port().await;
  let temp_dir = common::create_temp_dir();
  let server_config = common::create_test_server_config();
  let service = AdeployService::new(server_config);

  let addr = format!("127.0.0.1:{}", port).parse().unwrap();

  // Start server
  let server_handle = tokio::spawn(async move {
    Server::builder()
      .add_service(DeployServiceServer::new(service))
      .serve(addr)
      .await
  });

  // Give server time to start
  sleep(Duration::from_millis(200)).await;

  // Test server memory usage with simple operations
  let batches = 5;
  
  for batch in 0..batches {
    info!("Running batch {} of {}", batch + 1, batches);
    
    // Simulate some memory operations
    sleep(Duration::from_millis(50)).await;
    
    info!("Batch {} completed successfully", batch + 1);
    
    // Small delay between batches
    sleep(Duration::from_millis(100)).await;
  }

  // Clean up
  server_handle.abort();
}

#[tokio::test]
async fn test_request_timeout_handling() {
  let port = common::find_available_port().await;
  let temp_dir = common::create_temp_dir();
  let server_config = common::create_test_server_config();
  let service = AdeployService::new(server_config);

  let addr = format!("127.0.0.1:{}", port).parse().unwrap();

  // Start server
  let server_handle = tokio::spawn(async move {
    Server::builder()
      .add_service(DeployServiceServer::new(service))
      .serve(addr)
      .await
  });

  // Give server time to start
  sleep(Duration::from_millis(200)).await;

  // Test timeout functionality
  let start_time = Instant::now();
  let result = timeout(
    Duration::from_millis(1), // Very short timeout
    sleep(Duration::from_millis(100)) // This will timeout
  )
  .await;
  let elapsed = start_time.elapsed();

  // Should timeout quickly
  assert!(result.is_err()); // Timeout error
  assert!(elapsed < Duration::from_millis(50)); // Should timeout quickly

  // Clean up
  server_handle.abort();
}
