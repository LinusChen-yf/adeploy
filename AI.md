# ADeploy - Universal Deployment Tool

ADeploy is a universal deployment tool developed in Rust, supporting cross-platform and cross-language application deployment. It provides a concise and secure deployment solution through gRPC communication and TOML configuration files. The program is driven by Tokio and supports asynchronous deployment.

## Key Features

1. **Cross-platform support**: Supports major operating systems including Windows, Linux, and macOS
2. **Language agnostic**: Can deploy applications written in any language
3. **Simple configuration**: Uses TOML configuration files with simple and understandable syntax
4. **Secure transmission**: Ed25519 key-based authentication and encrypted transmission
5. **Unified tool**: Single binary file supporting both client and server modes
6. **Script support**: Supports executing custom scripts before/after deployment
7. **Backup functionality**: Optional backup feature with customizable backup paths
8. **Logging system**: Structured logging and rotation based on log2

## System Architecture

### Overall Architecture

```
┌─────────────────┐         gRPC         ┌─────────────────┐
│   Client Side   │◄────────────────────►│   Server Side   │
├─────────────────┤                      ├─────────────────┤
│ • client_config.│                      │ • server_config.│
│   toml          │                      │   toml          │
│ • SSH Key       │                      │ • Deploy Scripts│
│ • File Scanner  │                      │ • Backup System │
│ • gRPC Client   │                      │ • gRPC Server   │
└─────────────────┘                      └─────────────────┘
```

### Core Components

#### 1. Client Components
- **Configuration Parser**: Parses the `client_config.toml` configuration file
- **File Scanner**: Scans and packages files to be deployed
- **gRPC Client**: Communicates with the server
- **SSH Authentication**: Handles Ed25519 key authentication

#### 2. Server Components
- **gRPC Server**: Receives deployment requests
- **Configuration Manager**: Manages the `server_config.toml` configuration file
- **Deployment Executor**: Executes pre/post-deployment scripts
- **Backup Manager**: Optional backup functionality
- **Security Verifier**: Ed25519 key verification

## Configuration Design

### Client Configuration (client_config.toml)

```toml
# Package configuration, key is the package name
[packages.myapp1]
sources = ["./dist/myapp1"]

[packages.myapp2]
sources = ["./api-dist/myapp2"]

# Server configuration, key is the IP address
[servers."192.168.50.11"]
port = 6060
timeout = 30
key_path = ".key/id_ed25519.pub"

[servers."192.168.50.12"]
port = 8080
timeout = 60
key_path = ".key/id_ed25519.pub"

# Default server configuration
[servers.default]
port = 6060
key_path = ".key/id_ed25519.pub"
timeout = 30
```

### Server Configuration (server_config.toml)

```toml
[server]
port = 6060
max_file_size = 104857600  # 100MB in bytes
allowed_keys = [
    "AAAAC3NzaC1lZDI1NTE5AAAAI...",
    "AAAAC3NzaC1lZDI1NTE5AAAAA..."
]

# Package configuration, key is the package name
[packages.myapp1]
deploy_path = "/opt/myapp1"
pre_deploy_script = "./scripts/pre_deploy.sh"
post_deploy_script = "./scripts/post_deploy.sh"
backup_enabled = true
# Optional: Specify custom backup path, otherwise uses default path
backup_path = "/backup/myapp1"

[packages.myapp2]
deploy_path = "/opt/myapp2"
pre_deploy_script = "./scripts/pre_deploy.sh"
post_deploy_script = "./scripts/post_deploy.sh"
backup_enabled = false
```

### Configuration Usage Instructions

#### Configuration Structure Design

Configuration files use TOML's hierarchical structure with dot-separated key names to organize configurations:

- **Packages configuration**: Uses `[packages.name]` syntax, with each package's key being its `name`
- **Servers configuration**: Uses `[servers."IP address"]` syntax, with each server's key being its `IP address`

#### Package Configuration

Package configurations are defined using the `[packages.package-name]` syntax:

**Important notes:**
- The package's key (name) must be unique
- The `sources` field is a string array supporting multiple source paths
- Each source path supports both relative and absolute paths
- The program looks up corresponding configuration information by package name

#### Server Configuration

Server configurations are defined using the `[servers."IP address"]` syntax:

- **IP address as key**: Directly uses the target server's IP address as the configuration key
- **Default configuration**: `[servers.default]` serves as a fallback configuration when no specific configuration exists for the specified IP address
- **Configuration priority**: Command-line arguments > IP-specific configuration > default configuration
- The program looks up corresponding server configuration information by IP address

#### Usage Examples

```bash
# Deploy to 192.168.50.11, using the configuration for that IP
./adeploy 192.168.50.11 myapp1

# Deploy to 192.168.50.99, which will use the default configuration since no specific configuration exists
./adeploy 192.168.50.99 myapp1

# Specify configuration file (only available in explicit client mode)
./adeploy client 192.168.50.11 myapp1 -c ./custom_client_config.toml
```

## gRPC Service Interface Design

### Protocol Buffers Definition

```protobuf
syntax = "proto3";

package adeploy;

// Deploy service definition
service DeployService {
    rpc Deploy(DeployRequest) returns (DeployResponse);
}

// Deploy request message
message DeployRequest {
    string package_name = 1;
    string version = 2;
    bytes file_data = 3;
    string file_hash = 4;          // SHA256 hash for file integrity verification
    string signature = 5;
    string public_key = 6;
    map<string, string> metadata = 7;
}

// Deploy response message
message DeployResponse {
    bool success = 1;
    string message = 2;
    string deploy_id = 3;
    repeated string logs = 4;
}
```

## Security Mechanism Design

### Ed25519 Key Authentication Process

1. **Key Generation**: The client automatically generates an Ed25519 key pair on first run, stored by default in `.key/id_ed25519` (private key) and `.key/id_ed25519.pub` (public key)
2. **Public Key Registration**: The client's Ed25519 public key is added to the server configuration file's `allowed_keys` list
3. **Signature Verification**: The client signs requests using the private key
4. **Server Verification**: The server verifies signatures using registered public keys

### Transmission Security

- gRPC communication encrypted using TLS 1.3 (currently implemented with HTTP, can be upgraded to HTTPS/TLS in the future)
- File transmission uses streaming to support large files
- File integrity verified through SHA256 hashing

## Deployment Process Design

### Client Process

```
1. Read client_config.toml configuration
2. Select corresponding server configuration based on target IP address
3. Determine packages to deploy (all or specified)
4. Check if .key/id_ed25519 exists, generate new Ed25519 key pair if not
5. Precisely package files based on sources list in package configuration (supports files and directories)
6. Calculate SHA256 hash of the file package
7. Generate Ed25519 signature
8. Send gRPC request (containing file data and hash)
9. Wait for deployment results
10. Display deployment logs
```

### Server Process

```
1. Verify Ed25519 signature
2. Verify SHA256 hash of the file package
3. Check package configuration
4. Create backup (if enabled)
5. Execute pre_deploy script
6. Extract and deploy files to specified deploy_path directory
7. Set file permissions
8. Execute post_deploy script
9. Return deployment results
```

## Command-Line Interface Design

### Client Usage

```bash
# Basic deployment (deploy specified package to specified IP)
./adeploy 192.168.50.11 myapp1

# Explicit client mode
./adeploy client 192.168.50.11 myapp1

# Specify configuration file
./adeploy client 192.168.50.11 myapp1 -c ./custom_client_config.toml
```

### Server Usage

```bash
# Start server
./adeploy server

# Specify configuration file
./adeploy server -c ./custom_server_config.toml
```

## Error Handling and Logging

### Error Types

- **Configuration errors**: Configuration file format errors, missing required fields
- **Network errors**: Connection timeouts, network unreachable
- **Authentication errors**: Ed25519 key verification failures
- **Deployment errors**: Script execution failures, file permission issues
- **System errors**: Insufficient disk space, insufficient memory

### Logging System Design (based on log2)

#### Log Levels

- **ERROR**: Serious errors that cause operation failure
- **WARN**: Warning information, operations may be affected
- **INFO**: General information, records important operations
- **DEBUG**: Debug information, detailed execution process

#### Log Configuration

Uses the log2 library for log management

#### Log Format

Standard log format includes the following information:
- Timestamp (ISO 8601 format)
- Log level
- Module name
- Message content
- Optional context information (deploy_id, package_name, etc.)

Example log output:
```
2024-01-15T10:30:45.123Z [INFO] adeploy::server - Starting deploy server on port 6060
2024-01-15T10:31:02.456Z [INFO] adeploy::deploy - Received deploy request for package 'my-web-app' (deploy_id: abc123)
2024-01-15T10:31:03.789Z [DEBUG] adeploy::auth - Ed25519 key validation successful for client
2024-01-15T10:31:05.012Z [ERROR] adeploy::script - Pre-deploy script failed: exit code 1
```

#### Log Rotation Policy

- **Size limit**: Maximum 10MB per log file
- **Backup count**: Keep the most recent 5 backup files
- **Compression**: Automatically compress old log files (optional)
- **Cleanup**: Periodically clean up log files beyond retention period

## Performance Optimization

### File Transfer Optimization

- Uses streaming to handle large files
- Supports file compression (gzip)
- File integrity verification (SHA256 hash)
- Precise file transmission based on sources configuration

### Memory Management

- Uses memory mapping to handle large files
- Streaming processing to avoid memory overflow
- Releases temporary resources promptly

## Technology Stack

### Core Dependencies

- **tonic**: gRPC framework
- **toml**: TOML configuration file parsing
- **clap**: Command-line argument parsing
- **tokio**: Asynchronous runtime
- **serde**: Serialization/deserialization
- **log2**: Log recording and file output

### Auxiliary Dependencies

- **ed25519-dalek**: Ed25519 key generation and signature verification
- **tar**: File packaging
- **flate2**: File compression
- **uuid**: Unique identifier generation
- **chrono**: Time processing

## Deployment and Operations

### System Requirements

- **Operating System**: Linux, macOS, Windows
- **Memory**: Minimum 64MB, recommended 256MB
- **Disk**: Minimum 10MB, adjust based on deployment file size
- **Network**: TCP port access permissions

### Monitoring and Maintenance

- Deployment log monitoring
- System resource monitoring
- Error alerting mechanism
- Periodic backup cleanup