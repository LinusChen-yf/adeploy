use std::path::PathBuf; // PathBuf added for session_files_dir, Path for save_dir type hint
use std::{error::Error, fs, path::Path};

use log2::{error, info};
use rhai::{Engine, Scope};
use tokio::{
  fs::File as TokioFile,
  io::{AsyncReadExt, BufReader},
  net::{TcpListener, TcpStream},
};
use uuid::Uuid;

use crate::rhai_utils::{self, register_platform_functions, register_update_binary}; // Added for unique session IDs

const PORT: u16 = 4441;
const RECEIVED_FILES_DIR: &str = "deploy_files";

pub async fn run_server() -> Result<(), Box<dyn Error>> {
  let addr = format!("0.0.0.0:{}", PORT);
  let listener = TcpListener::bind(&addr).await?;
  info!("Server: Listening for incoming connections on {}", addr);

  loop {
    match listener.accept().await {
      Ok((stream, _)) => {
        tokio::spawn(async move {
          if let Err(e) = handle_connection(stream).await {
            error!("Failed to handle connection: {}", e);
          }
        });
      }
      Err(e) => {
        error!("Failed to accept connection: {}", e);
      }
    }
  }
}

async fn receive_file(
  stream: &mut TcpStream,
  file_description: &str,
  save_dir: &Path, // New parameter: directory to save the file in
) -> Result<PathBuf, Box<dyn Error>> {
  // 1. Read filename length (u32)
  let filename_len = stream.read_u32().await?;

  // 2. Read filename
  let mut filename_bytes = vec![0u8; filename_len as usize];
  stream.read_exact(&mut filename_bytes).await?;
  let filename_str = String::from_utf8(filename_bytes)?;

  // Sanitize filename to prevent path traversal and use only the base name
  let filename = Path::new(&filename_str)
    .file_name()
    .ok_or("Invalid filename received from client")?
    .to_string_lossy()
    .into_owned();

  // 3. Read file content length (u64)
  let file_content_len = stream.read_u64().await?;

  // The save_dir is expected to exist, created by handle_connection.
  let save_path = save_dir.join(&filename);

  // 4. Create file and write content
  let mut file = TokioFile::create(&save_path).await?;

  // Use tokio::io::copy to stream data from the socket to the file, limited by file_content_len
  let mut reader = BufReader::new(stream.take(file_content_len));
  let bytes_copied = tokio::io::copy(&mut reader, &mut file).await?;

  info!(
    "Server: Successfully received and saved {} ({} bytes) to: {:?}",
    file_description, bytes_copied, save_path
  );
  Ok(save_path)
}

async fn handle_connection(mut stream: TcpStream) -> Result<(), Box<dyn Error>> {
  let peer_addr = stream.peer_addr()?;
  info!("Accepted connection from: {}", peer_addr);

  // 1. Create unique temporary directory for this session
  let session_id = Uuid::new_v4().to_string();
  let session_files_dir = PathBuf::from(RECEIVED_FILES_DIR).join(&session_id);
  fs::create_dir_all(&session_files_dir)?;

  // Wrap the main logic in a block to ensure cleanup
  let result = async {
    // Receive the Rhai script file into the session directory
    let script_file_path = receive_file(&mut stream, "Rhai script", &session_files_dir).await?;

    // Receive the source program file into the session directory
    let source_path = receive_file(&mut stream, "source program", &session_files_dir).await?;

    let mut engine = Engine::new();
    register_platform_functions(&mut engine);
    engine.on_print(|content| {
      info!("[Rhai] {}", content);
    });
    let ast = engine.compile_file(script_file_path.clone())?;

    // 2. Parse Rhai script to get config (especially target_path)
    let target_path_str = rhai_utils::parse_target_path(&engine, &ast)?;
    let target_path = PathBuf::from(target_path_str);
    if !target_path.exists() {
      error!("target_path does not exist");
    }
    info!("Received deploy files successfully.");

    // 3. Register platform functions, making update_binary use server-side context
    register_update_binary(&mut engine, source_path.clone(), target_path.clone());

    info!("Executing Rhai script...");

    let mut scope = Scope::new();
    match engine.call_fn::<()>(&mut scope, &ast, "deploy", ()) {
      Ok(_) => info!("Rhai script executed successfully with deployment actions."),
      Err(e) => {
        return Err(format!("Error executing Rhai script with deployment actions: {}", e).into());
      }
    }

    Ok(())
  }
  .await;

  // 4. Cleanup temporary directory
  fs::remove_dir_all(&session_files_dir)?;

  result
}
