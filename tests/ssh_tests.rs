//! Ed25519 authentication tests

use adeploy::{
  auth::Auth,
};

mod common;

#[test]
fn test_ed25519_signature_verification() {
  let temp_dir = common::create_temp_dir();
  let private_key_path = temp_dir.path().join("test_key");
  let public_key_path = temp_dir.path().join("test_key.pub");
  
  // Generate Ed25519 key pair
  let result = Auth::generate_key_pair(
    &public_key_path.to_string_lossy(),
    &private_key_path.to_string_lossy()
  );
  assert!(result.is_ok());
  
  // Load the keypair
  let keypair = Auth::load_key_pair(&private_key_path.to_string_lossy()).unwrap();
  let auth = Auth::with_key_pair(keypair);
  
  // Load public key
  let public_key = std::fs::read_to_string(&public_key_path).unwrap();
  
  // Test data to sign
  let test_data = b"test data for signing";
  
  // Create a proper signature
  let signature = auth.sign_data(test_data).unwrap();
  
  // Test signature verification
  let verification_result = Auth::verify_signature(&public_key, test_data, &signature);
  assert!(verification_result.is_ok());
  
  // This should return true since we're using the correct signature
  assert!(verification_result.unwrap());
  
  // Test with incorrect signature
  let wrong_signature = vec![0; 64]; // All zeros (Ed25519 signatures are 64 bytes)
  let verification_result = Auth::verify_signature(&public_key, test_data, &wrong_signature);
  assert!(verification_result.is_ok());
  assert!(!verification_result.unwrap());
}
