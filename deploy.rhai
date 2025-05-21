// deploy.rhai

const source_path = "./target/debug/adeploy";
const target_path = "./deploy/adeploy";

fn get_source_path() {
  global::source_path
}

fn get_target_path() {
  global::target_path
}

fn deploy() {
  print("Executing deployment actions...");
  let stop_result = stop_service("ADeploy");
  if stop_result != true {
    throw "Error stopping service: " + stop_result;
  }

  sleep(1);

  let update_result = update_binary();
  if update_result != true {
    throw "Error updating binary: " + update_result;
  }

  // Attempt to start a service (e.g., the newly deployed application)
  let start_result = start_service("ADeploy");
  if start_result != true {
    throw "Error starting service: " + start_result;
  }
  print("Deployment script finished its custom actions.");
}