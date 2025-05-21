# adeploy

adeploy (any deploy) is a cross-platform application deployment tool. Custom deployment logic is implemented through Rhai scripts.

It is inspired by the `cros deploy` tool from Chromium OS.

## Features

- Cross-platform
- Customizable deployment logic through Rhai scripts

## Todo list
- [x] Customizable deployment logic through Rhai scripts
- [x] Support cross-platform client
- [x] Support Windows Server
- [ ] Support Linux Server
- [ ] Support macOS Server
- [ ] Support more built-in functions
- [ ] Support server as a library to integrate with your program

## How to Use

### Server Side

```
# Start server
> adeploy server
```

### Client Side

```
# Deploy application (using default deploy.rhai in current directory)
> adeploy <server_ip>

# Deploy application (using a specific script file)
> adeploy <server_ip> <path_to_your_script.rhai>
```

### rhai script example

```
const source_path = "./target/debug/adeploy";
const target_path = "./deploy/adeploy";

// required
fn get_source_path() {
  global::source_path
}

// required
fn get_target_path() {
  global::target_path
}

// required
fn deploy() {
  // deploy logic goes here
  // ...
  stop_service("adeploy");
  update_binary();
  start_service("adeploy");
}
```

#### Built-in functions

```
// Stop process
fn stop_process(process_name: String);
// Stop Service
fn stop_service(service_name: String);
// Start Service
fn start_service(service_name: String);
// Update binary
fn update_binary();
```
