// src/platform_functions/windows.rs

use std::process::Command;

use log2::warn;
use rhai::Dynamic;

pub fn stop_process_impl(process_name: String) -> Result<(), String> {
  let output = Command::new("taskkill")
    .args(&["/F", "/IM", &process_name])
    .output();

  match output {
    Ok(output) => {
      if output.status.success() {
        Ok(())
      } else {
        Err(format!(
          "Failed to stop process '{}' on Windows. Exit code: {}. Stderr: {}",
          process_name,
          output.status,
          String::from_utf8_lossy(&output.stderr)
        ))
      }
    }
    Err(e) => Err(format!(
      "Error executing taskkill for process '{}' on Windows: {}",
      process_name, e
    )),
  }
}

pub fn start_service_impl(service_name: String) -> rhai::Dynamic {
  let output = Command::new("sc").args(&["start", &service_name]).output();
  match output {
    Ok(output) => {
      if output.status.success() {
        Dynamic::from(true)
      } else {
        Dynamic::from(format!(
          "Failed to start service '{}' on Windows. Exit code: {}. Stderr: {}",
          service_name,
          output.status,
          String::from_utf8_lossy(&output.stderr)
        ))
      }
    }
    Err(e) => Dynamic::from(format!(
      "Error executing sc start for service '{}' on Windows: {}",
      service_name, e
    )),
  }
}

pub fn stop_service_impl(service_name: String) -> rhai::Dynamic {
  let output = Command::new("sc").args(&["stop", &service_name]).output();
  match output {
    Ok(output) => {
      if output.status.success() {
        Dynamic::from(true)
      } else {
        if output.status.code() == Some(1062) {
          warn!("Service '{}' is already stopped.", service_name);
          return Dynamic::from(true);
        }
        Dynamic::from(format!(
          "Failed to stop service '{}' on Windows. Exit code: {}. Stderr: {}",
          service_name,
          output.status,
          String::from_utf8_lossy(&output.stderr)
        ))
      }
    }
    Err(e) => Dynamic::from(format!(
      "Error executing sc stop for service '{}' on Windows: {}",
      service_name, e
    )),
  }
}
