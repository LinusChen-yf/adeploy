use std::path::PathBuf;

use adeploy::platform_functions;
use anyhow::{bail, Result};
use log2::info;
use rhai::{Engine, Scope, AST};

pub fn register_platform_functions(engine: &mut Engine) {
  engine.register_fn("stop_process", platform_functions::stop_process);
  engine.register_fn("start_service", platform_functions::start_service);
  engine.register_fn("stop_service", platform_functions::stop_service);
  engine.register_fn("get_dir_entries", platform_functions::get_dir_entries);
}

pub fn register_update_binary(engine: &mut Engine, source_path: PathBuf, target_path: PathBuf) {
  engine.register_fn("update_binary", move || {
    info!(
      "Server: Rhai script called 'update_binary'. Moving '{}' to '{}'",
      &source_path.display(),
      &target_path.display()
    );
    platform_functions::update_binary(source_path.clone(), target_path.clone())
  });
}

pub fn parse_source_path(engine: &Engine, ast: &AST) -> Result<PathBuf> {
  // Call rhai function
  let mut scope = Scope::new();
  let source_path_str = match engine.call_fn::<String>(&mut scope, &ast, "get_source_path", ()) {
    Ok(result) => result,
    Err(e) => bail!("Failed to call 'get_source_path' function: {}", e),
  };

  // Get the directory of the script
  let script_path = PathBuf::from(source_path_str);

  // Canonicalize the source path
  let script_path = match script_path.canonicalize() {
    Ok(path) => path,
    Err(e) => bail!(
      "Failed to canonicalize source path '{}': {}",
      script_path.display(),
      e
    ),
  };

  if !script_path.exists() {
    bail!("Source path does not exist: {}", script_path.display())
  }

  Ok(script_path)
}

pub fn parse_target_path(engine: &Engine, ast: &AST) -> Result<String> {
  // Create a scope
  let mut scope = Scope::new();

  let target_path_str = match engine.call_fn::<String>(&mut scope, &ast, "get_target_path", ()) {
    Ok(result) => result,
    Err(e) => bail!("Failed to call 'get_target_path' function: {}", e),
  };

  Ok(target_path_str)
}
