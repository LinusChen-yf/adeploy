// src/platform_functions/linux.rs

use std::{fs, process::Command};

pub fn stop_process_impl(process_name: String) -> Result<(), String> {
  let output = Command::new("pkill").arg(&process_name).output();

  match output {
    Ok(output) => {
      if output.status.success() {
        Ok(())
      } else {
        // pkill might return a non-zero exit code if no process was found
        // Consider if this should be an error or not based on desired behavior.
        Err(format!(
          "Failed to stop process '{}' on Linux or process not found. Exit code: {}. Stderr: {}",
          process_name,
          output.status,
          String::from_utf8_lossy(&output.stderr)
        ))
      }
    }
    Err(e) => Err(format!(
      "Error executing pkill for process '{}' on Linux: {}",
      process_name, e
    )),
  }
}

pub fn start_service_impl(service_name: String) -> Result<(), String> {
  let output = Command::new("systemctl")
    .args(&["start", &service_name])
    .output();

  match output {
    Ok(output) => {
      if output.status.success() {
        Ok(())
      } else {
        Err(format!(
          "Failed to start service '{}' on Linux. Exit code: {}. Stderr: {}",
          service_name,
          output.status,
          String::from_utf8_lossy(&output.stderr)
        ))
      }
    }
    Err(e) => Err(format!(
      "Error executing systemctl start for service '{}' on Linux: {}",
      service_name, e
    )),
  }
}
