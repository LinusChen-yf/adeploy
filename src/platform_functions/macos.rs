// src/platform_functions/macos.rs

use std::process::Command;

pub fn stop_process_impl(process_name: String) -> Result<(), String> {
  let output = Command::new("pkill").arg(&process_name).output();

  match output {
    Ok(output) => {
      if output.status.success() {
        Ok(())
      } else {
        // pkill might return a non-zero exit code if no process was found
        Err(format!(
          "Failed to stop process '{}' on macOS or process not found. Exit code: {}. Stderr: {}",
          process_name,
          output.status,
          String::from_utf8_lossy(&output.stderr)
        ))
      }
    }
    Err(e) => Err(format!(
      "Error executing pkill for process '{}' on macOS: {}",
      process_name, e
    )),
  }
}

pub fn start_service_impl(service_name: String) -> Result<(), String> {
  let output = Command::new("launchctl")
    .args(&["start", &service_name])
    .output();

  match output {
    Ok(output) => {
      if output.status.success() {
        Ok(())
      } else {
        Err(format!(
          "Failed to start service '{}' on macOS. Exit code: {}. Stderr: {}. stdout: {}",
          service_name,
          output.status,
          String::from_utf8_lossy(&output.stderr),
          String::from_utf8_lossy(&output.stdout)
        ))
      }
    }
    Err(e) => Err(format!(
      "Error executing launchctl start for service '{}' on macOS: {}",
      service_name, e
    )),
  }
}
