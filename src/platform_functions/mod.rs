// src/platform_functions/mod.rs

use std::{fs, path::PathBuf};

use log2::warn;
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
    Dynamic::from(format!(
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
  match fs::copy(&source_path, &target_path) {
    Ok(_) => Dynamic::from(true),
    Err(e) => {
      // fs::rename might fail across different drives. A copy and delete might be more robust.
      Dynamic::from(format!(
                "Failed to update binary from '{}' to '{}' on Windows: {}. Consider implementing a copy-then-delete strategy for cross-drive moves.",
                source_path.display(),
                target_path.display(),
                e
            ))
    }
  }
}

pub fn get_dir_entries(path_str: String) -> rhai::Dynamic {
  let path = PathBuf::from(&path_str);
  if !path.exists() {
    return Dynamic::from(format!("Path '{}' does not exist.", path_str));
  }
  if !path.is_dir() {
    return Dynamic::from(format!("Path '{}' is not a directory.", path_str));
  }
  let mut file_list = rhai::Array::new();

  let files = match path.read_dir() {
    Ok(files) => files,
    Err(e) => return Dynamic::from(format!("Failed to read directory '{}': {}", path_str, e)),
  };
  for file in files {
    let file = match file {
      Ok(file) => file,
      Err(e) => {
        warn!("Failed to read file: {}", e);
        continue;
      }
    };
    let path = file.path();
    let mut file_info = rhai::Map::new();
    file_info.insert(
      "name".into(),
      Dynamic::from(file.file_name().to_string_lossy().to_string()),
    );
    file_info.insert("is_dir".into(), Dynamic::from(path.is_dir()));
    file_list.push(Dynamic::from(file_info));
  }
  Dynamic::from(file_list)
}
