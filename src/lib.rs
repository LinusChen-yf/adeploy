//! ADeploy - Universal deployment tool library

pub mod auth;
pub mod client;
pub mod config;
pub mod deploy;
pub mod deploy_log;
pub mod error;
pub mod server;

// Include the generated gRPC code
pub mod adeploy {
  tonic::include_proto!("adeploy");
}
