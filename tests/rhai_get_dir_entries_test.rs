use std::{
  fs::{self, File},
  io::Write,
  path::PathBuf,
};

use adeploy::platform_functions;
use rhai::{Dynamic, Engine, Scope};

// Helper function to create a Rhai engine with the get_dir_entries function registered.
fn create_engine() -> Engine {
  let mut engine = Engine::new();
  engine.register_fn("get_dir_entries", platform_functions::get_dir_entries);
  engine
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;

  use super::*;

  // Helper function to create a temporary directory with some files and subdirectories.
  fn setup_test_dir(base_path: &PathBuf) -> std::io::Result<()> {
    fs::create_dir_all(base_path.join("subdir"))?;
    File::create(base_path.join("file1.txt"))?.write_all(b"hello")?;
    File::create(base_path.join("subdir/file2.txt"))?.write_all(b"world")?;
    Ok(())
  }

  // Helper function to clean up the temporary directory.
  fn cleanup_test_dir(base_path: &PathBuf) -> std::io::Result<()> {
    if base_path.exists() {
      fs::remove_dir_all(base_path)?;
    }
    Ok(())
  }

  #[test]
  fn test_get_dir_entries_existing_directory() {
    let engine = create_engine();
    let mut scope = Scope::new();

    let test_dir_name = "test_get_dir_entries";
    let mut temp_dir = std::env::temp_dir();
    temp_dir.push(test_dir_name);

    // Cleanup before test, in case previous run failed
    let _ = cleanup_test_dir(&temp_dir);
    setup_test_dir(&temp_dir).expect("Failed to set up test directory");

    let script = format!(
      r#"
            let files = get_dir_entries("{}");
            for item in files {{
                print("Name: " + item.name + ", Is Dir: " + item.is_dir);
            }}
            files
            "#,
      temp_dir.to_str().unwrap().replace("\\", "/") // Ensure forward slashes for Rhai path
    );

    let result = engine
      .eval_with_scope::<Dynamic>(&mut scope, &script)
      .expect("Failed to evaluate script");

    assert!(result.is_array(), "Result should be an array");
    let files_array = result.into_array().unwrap();

    // Expected files and directories
    let mut expected_items = HashMap::new();
    expected_items.insert("file1.txt".to_string(), false);
    expected_items.insert("subdir".to_string(), true);

    assert_eq!(
      files_array.len(),
      expected_items.len(),
      "Should list the correct number of items"
    );

    for file in files_array {
      let map = file.as_map_ref().expect("sss");
      let name = map
        .get("name")
        .expect("Map should have 'name'")
        .clone()
        .into_string()
        .expect("'name' should be a string");
      let is_dir = map
        .get("is_dir")
        .expect("Map should have 'is_dir'")
        .clone()
        .as_bool()
        .expect("'is_dir' should be a boolean");

      assert!(
        expected_items.contains_key(&name),
        "Unexpected item: {}",
        name
      );
      assert_eq!(
        expected_items[&name], is_dir,
        "Mismatch in is_dir for item: {}",
        name
      );
    }

    cleanup_test_dir(&temp_dir).expect("Failed to clean up test directory");
  }

  #[test]
  fn test_get_dir_entries_non_existent_path() {
    let engine = create_engine();
    let mut scope = Scope::new();
    let non_existent_path = "./non_existent_path_for_test";

    let script = format!(
      r#"
            get_dir_entries("{}")
            "#,
      non_existent_path
    );

    let result = engine
      .eval_with_scope::<Dynamic>(&mut scope, &script)
      .expect("Failed to evaluate script for non-existent path");
    assert!(
      result.is_string(),
      "Result should be a string for non-existent path"
    );
    let error_message = result.into_string().unwrap();
    assert!(
      error_message.contains("does not exist"),
      "Error message should indicate path does not exist. Got: {}",
      error_message
    );
  }

  #[test]
  fn test_get_dir_entries_with_file_path() {
    let engine = create_engine();
    let mut scope = Scope::new();

    let test_file_name = "test_file_for_get_dir_entries.txt";
    let mut temp_file_path = std::env::temp_dir();
    temp_file_path.push(test_file_name);

    File::create(&temp_file_path)
      .expect("Failed to create temp file")
      .write_all(b"test")
      .expect("Failed to write to temp file");

    let script = format!(
      r#"
            get_dir_entries("{}")
            "#,
      temp_file_path.to_str().unwrap().replace("\\", "/")
    );

    let result = engine
      .eval_with_scope::<Dynamic>(&mut scope, &script)
      .expect("Failed to evaluate script for file path");
    assert!(
      result.is_string(),
      "Result should be a string when path is a file"
    );
    let error_message = result.into_string().unwrap();
    assert!(
      error_message.contains("is not a directory"),
      "Error message should indicate path is not a directory. Got: {}",
      error_message
    );

    fs::remove_file(&temp_file_path).expect("Failed to remove temp file");
  }
}
