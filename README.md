# ADeploy - Universal Deployment Tool

ADeploy is a universal deployment tool developed in Rust, supporting cross-platform and cross-language application deployment. It provides a concise and secure deployment solution through gRPC communication and TOML configuration files.

## Key Features

- Cross-platform support (Windows, Linux, macOS)
- Language agnostic - can deploy applications written in any language
- Simple TOML configuration files
- SSH key-based authentication and encrypted transmission
- Support for pre/post-deployment script execution
- Backup functionality (with customizable backup paths)

## Configuration

ADeploy uses TOML configuration files for both client and server setups. There are two main configuration files:

1. **Client Configuration** (`client_config.toml`) - Defines packages to deploy and server connection details
2. **Server Configuration** (`server_config.toml`) - Defines server settings and package deployment parameters

### Client Configuration (client_config.toml)

The client configuration file defines the packages to be deployed and server connection details.

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

[servers."192.168.50.12"]
port = 8080
timeout = 60

# Default server configuration
[servers.default]
port = 6060
key_path = ".key/id_ed25519.pub"
timeout = 30
```

#### Package Configuration

Each package is defined under `[packages.package-name]` with the following options:

- `sources` - An array of file/directory paths to include in the deployment package. Supports both relative and absolute paths.

#### Server Configuration

Server configurations are defined under `[servers."IP-address"]` with the following options:

- `port` - The port number for the server (default: 6060)
- `timeout` - Connection timeout in seconds
- `key_path` - Path to the SSH public key file

### Server Configuration (server_config.toml)

The server configuration file defines server settings and deployment parameters for each package.

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

#### Server Settings

- `port` - The port number for the server to listen on (default: 6060)
- `max_file_size` - Maximum allowed file size in bytes (default: 100MB)
- `allowed_keys` - List of authorized SSH public keys for client authentication

#### Package Deployment Configuration

Each package deployment is defined under `[packages.package-name]` with the following options:

- `deploy_path` - The target directory where the package will be deployed
- `pre_deploy_script` - Script to execute before deployment (optional)
- `post_deploy_script` - Script to execute after deployment (optional)
- `backup_enabled` - Whether to enable backup functionality (true/false)
- `backup_path` - Optional custom backup path (if not specified, backups are stored in a default location)

##### Backup Path Options

- If `backup_path` is not specified, backups will be stored in a subdirectory named after the package within the executable's directory
- If `backup_path` is specified, backups will be stored in the specified directory

### Configuration Usage Examples

```bash
# Deploy to 192.168.50.11, using the configuration for that IP
./adeploy 192.168.50.11

# Deploy to 192.168.50.99, which will use the default configuration
./adeploy 192.168.50.99

# Deploy only the specified package
./adeploy 192.168.50.11 --package my-web-app
# or
./adeploy 192.168.50.11 my-api-app
```