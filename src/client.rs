use std::{error::Error, path::Path};

use log2::{error, info};
use tokio::{fs::File as TokioFile, io::AsyncWriteExt, net::TcpStream};

const SERVER_PORT: u16 = 4441;

async fn send_file(
  stream: &mut TcpStream,
  file_path: &Path,
  file_description: &str,
) -> Result<(), Box<dyn Error>> {
  let filename = file_path
    .file_name()
    .ok_or_else(|| format!("{} does not have a filename.", file_description))?
    .to_str()
    .ok_or_else(|| format!("{} filename contains invalid UTF-8.", file_description))?;

  // 1. Send filename length (u32)
  let filename_bytes = filename.as_bytes();
  stream.write_u32(filename_bytes.len() as u32).await?;

  // 2. Send filename
  stream.write_all(filename_bytes).await?;

  // 3. Open file and get its size
  let mut file = TokioFile::open(&file_path).await?;
  let metadata = file.metadata().await?;
  let file_size = metadata.len();

  // 4. Send file content length (u64)
  stream.write_u64(file_size).await?;

  // 5. Send file content
  let bytes_copied = tokio::io::copy(&mut file, stream).await?;
  if bytes_copied != file_size {
    error!(
            "Warning: For {}, bytes copied ({}) does not match file size ({}). This might indicate an issue.",
            file_description, bytes_copied, file_size
        );
  }
  Ok(())
}

pub async fn run_client(
  ip_address: &str,
  script_path: &Path,
  source_path: &Path,
) -> Result<(), Box<dyn Error>> {
  let server_addr = format!("{}:{}", ip_address, SERVER_PORT);
  let mut stream = TcpStream::connect(&server_addr).await?;
  info!("Client: Connected to server.");

  // Send the Rhai script file
  send_file(&mut stream, script_path, "Rhai script").await?;

  // Send the source program file
  send_file(&mut stream, source_path, "source program").await?;

  // Ensure all data is sent before closing
  stream.shutdown().await?;

  info!(
    "Client: All files ('{}' and '{}') sent successfully to {}.",
    script_path.display(),
    source_path.display(),
    ip_address
  );
  Ok(())
}
