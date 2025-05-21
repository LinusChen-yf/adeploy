// src/platform_functions/mod.rs

use std::{fs, path::PathBuf};

use rhai::Dynamic;

// Declare platform-specific modules
#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

// Define common functions that will call platform-specific implementations

/// Stops a process by its name.
/// Returns Ok(()) if successful, or an Err(String) with an error message.
pub fn stop_process(process_name: String) -> Result<(), String> {
  #[cfg(target_os = "windows")]
  {
    windows::stop_process_impl(process_name)
  }
  #[cfg(target_os = "linux")]
  {
    Err(format!(
      "stop_process not yet implemented for Linux for process: {}",
      process_name
    ))
  }
  #[cfg(target_os = "macos")]
  {
    Err(format!(
      "stop_process not yet implemented for macOS for process: {}",
      process_name
    ))
  }
  #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
  {
    Err(format!(
      "stop_process is not supported on this OS for process: {}",
      process_name
    ))
  }
}

pub fn start_service(service_name: String) -> rhai::Dynamic {
  #[cfg(target_os = "windows")]
  {
    windows::start_service_impl(service_name)
  }
  #[cfg(target_os = "linux")]
  {
    Dynamic::from(format!(
      "start_service not yet implemented for Linux for service: {}",
      service_name
    ))
  }
  #[cfg(target_os = "macos")]
  {
    Dynamic::from(format!(
      "start_service not yet implemented for macOS for service: {}",
      service_name
    ))
  }
  #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
  {
    Dynamic::from(format!(
      "start_service is not supported on this OS for service: {}",
      service_name
    ))
  }
}

/// Stop a service by its name.
/// Returns Ok(()) if successful, or an Err(String) with an error message.
pub fn stop_service(service_name: String) -> rhai::Dynamic {
  #[cfg(target_os = "windows")]
  {
    windows::stop_service_impl(service_name)
  }
  #[cfg(target_os = "linux")]
  {
    Err(format!(
      "stop_service not yet implemented for Linux for service: {}",
      service_name
    ))
  }
  #[cfg(target_os = "macos")]
  {
    Dynamic::from(format!(
      "stop_service not yet implemented for macOS for service: {}",
      service_name
    ))
  }
  #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
  {
    Dynamic::from(format!(
      "stop_service is not supported on this OS for service: {}",
      service_name
    ))
  }
}

/// Moves a file from source to target path.
pub fn update_binary(source_path: PathBuf, target_path: PathBuf) -> rhai::Dynamic {
  let target_exe = target_path.join("test.exe");
  match fs::copy(&source_path, &target_exe) {
    Ok(_) => Dynamic::from(true),
    Err(e) => {
      // fs::rename might fail across different drives. A copy and delete might be more robust.
      Dynamic::from(format!(
                "Failed to update binary from '{}' to '{}' on Windows: {}. Consider implementing a copy-then-delete strategy for cross-drive moves.",
                source_path.display(),
                target_exe.display(),
                e
            ))
    }
  }
}
