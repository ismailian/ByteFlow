//! End-to-end vault state machine + the non-negotiable wipe assertions
//! (spec §16 "Security"). No network: nothing here talks to R2.

use std::sync::Arc;

use bf_app::NoopNotifier;
use bf_app::auth::{AuthController, GENERIC_UNLOCK_ERROR, VaultState};
use bf_app::core::Core;
use bf_data::paths::AppPaths;
use bf_data::r2::R2Config;
use bf_data::secure::SecureStore;

fn secret(s: &str) -> bf_core::crypto::SecretBytes {
    zeroize::Zeroizing::new(s.as_bytes().to_vec())
}

fn cfg() -> R2Config {
    R2Config {
        account_id: "acc".into(),
        access_key_id: "AKIDEXAMPLE".into(),
        secret_access_key: "secret-key".into(),
        bucket: "bucket".into(),
        public_url: None,
        endpoint: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn onboarding_unlock_and_destruction_wipe() {
    let root = tempfile::tempdir().unwrap();
    let documents = root.path().join("Documents");
    std::fs::create_dir_all(&documents).unwrap();
    std::fs::write(documents.join("keep.txt"), b"user data").unwrap();
    let paths = AppPaths::at(root.path().join("support"), root.path().join("cache"), root.path().join("temp")).unwrap();

    let core = Core::open(paths.clone(), Arc::new(NoopNotifier)).await.unwrap();
    let auth = AuthController::new(core.clone());
    auth.init().await;
    assert_eq!(auth.state(), VaultState::NeedsPasswords);

    // Rules: the two passwords must differ.
    assert!(auth.set_passwords(secret("same-password"), secret("same-password")).await.is_err());
    auth.set_passwords(secret("login-password-1"), secret("destroy-password-1")).await.unwrap();
    assert_eq!(auth.state(), VaultState::NeedsConfig);
    auth.save_config(cfg()).await.unwrap();
    assert_eq!(auth.state(), VaultState::Unlocked);
    assert_eq!(auth.session().unwrap().config(), cfg());

    // Some state that the wipe must remove.
    core.thumbs.put("photo.jpg", b"jpeg").unwrap();
    std::fs::write(core.paths.temp_sub("enc_preview").join("x"), b"plaintext").unwrap();
    std::fs::write(core.paths.encryption_state(), b"{}").unwrap();

    auth.lock().await;
    assert_eq!(auth.state(), VaultState::Locked { error: None });
    assert!(auth.session().is_none());

    // Wrong password: generic error.
    auth.submit_password(secret("not-the-password")).await;
    assert_eq!(auth.state(), VaultState::Locked { error: Some(GENERIC_UNLOCK_ERROR.into()) });

    // Correct password unlocks.
    auth.submit_password(secret("login-password-1")).await;
    assert_eq!(auth.state(), VaultState::Unlocked);
    auth.lock().await;

    // Destruction password: silent wipe → fresh-install onboarding.
    auth.submit_password(secret("destroy-password-1")).await;
    assert_eq!(auth.state(), VaultState::NeedsPasswords);

    assert!(core.secure.keys().unwrap().is_empty(), "secure storage readAll() is empty");
    assert!(!paths.secure_store().exists(), "store file removed");
    assert!(!core.credentials.is_configured().unwrap());
    assert!(!core.thumbs.has("photo.jpg"), "thumbnail cache gone");
    assert!(!paths.thumbs().join("00").exists());
    assert!(!paths.encryption_state().exists());
    assert!(!paths.temp.join("enc_preview").join("x").exists());
    // The app DB was deleted then recreated empty (fresh install).
    assert!(core.app_db.transfers().await.unwrap().is_empty());
    // Documents untouched.
    assert_eq!(std::fs::read(documents.join("keep.txt")).unwrap(), b"user data");

    // Resumable: a tombstone left behind re-runs the wipe at launch.
    core.vault.set_wipe_pending().unwrap();
    core.secure.write("credential_blob", b"x").unwrap();
    let auth2 = AuthController::new(core.clone());
    auth2.init().await;
    assert_eq!(auth2.state(), VaultState::NeedsPasswords);
    assert!(core.secure.keys().unwrap().is_empty());
}
