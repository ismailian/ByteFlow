//! End-to-end vault state machine + the non-negotiable wipe assertions
//! (spec §16 "Security"). No network: nothing here talks to R2.

use std::sync::Arc;

use bf_app::NoopNotifier;
use bf_app::auth::{AuthController, GENERIC_UNLOCK_ERROR, VaultState};
use bf_app::buckets::PRIMARY_BUCKET_ID;
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

    // Onboarding asks only for the login password (destruction is optional).
    assert!(auth.set_login_password(secret("short")).await.is_err());
    auth.set_login_password(secret("login-password-1")).await.unwrap();
    assert_eq!(auth.state(), VaultState::NeedsConfig);
    auth.save_config(cfg()).await.unwrap();
    assert_eq!(auth.state(), VaultState::Unlocked);
    // Onboarding opens the new bucket straight away.
    assert_eq!(auth.session().unwrap().config(), cfg());
    assert!(!auth.destruction_enabled().await, "fresh installs have no destruction password");

    // Turning it on: the login password authorises; the two must differ.
    assert!(auth.set_destruction_password(secret("login-password-1"), secret("login-password-1")).await.is_err());
    assert!(auth.set_destruction_password(secret("wrong-login-pw"), secret("destroy-password-1")).await.is_err());
    auth.set_destruction_password(secret("login-password-1"), secret("destroy-password-1")).await.unwrap();
    assert!(auth.destruction_enabled().await);

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

    // Correct password unlocks to the bucket grid (no bucket open yet).
    auth.submit_password(secret("login-password-1")).await;
    assert_eq!(auth.state(), VaultState::Unlocked);
    assert!(auth.session().is_none());
    assert_eq!(auth.buckets().len(), 1);
    auth.open_bucket(PRIMARY_BUCKET_ID).await.unwrap();
    assert_eq!(auth.session().unwrap().bucket_id, PRIMARY_BUCKET_ID);

    // A second bucket: its own stores, keys and listing cache.
    let mut second = cfg();
    second.bucket = "second-bucket".into();
    assert!(auth.add_bucket(secret("wrong-login-pw"), second.clone()).await.is_err());
    assert!(auth.add_bucket(secret("login-password-1"), cfg()).await.is_err(), "duplicate");
    let id = auth.add_bucket(secret("login-password-1"), second.clone()).await.unwrap();
    assert_eq!(auth.buckets().len(), 2);
    auth.open_bucket(&id).await.unwrap();
    let s2 = auth.session().unwrap();
    assert_eq!(s2.config(), second);
    assert!(s2.stores.cache_db.path().starts_with(paths.bucket_dir(&id)));
    s2.stores.thumbs.put("only-in-second.jpg", b"jpeg").unwrap();
    assert!(!core.thumbs.has("only-in-second.jpg"), "thumbnails are per bucket");
    drop(s2);
    auth.close_bucket().await;
    assert!(auth.session().is_none());

    // The bucket list survives a lock.
    auth.lock().await;
    assert!(auth.buckets().is_empty());
    auth.submit_password(secret("login-password-1")).await;
    assert_eq!(auth.buckets().len(), 2);
    // Removing erases its local data; the last bucket can't be removed.
    auth.remove_bucket(secret("login-password-1"), &id).await.unwrap();
    assert_eq!(auth.buckets().len(), 1);
    assert!(!paths.bucket_dir(&id).exists());
    assert!(auth.remove_bucket(secret("login-password-1"), PRIMARY_BUCKET_ID).await.is_err());
    auth.lock().await;

    // Turning the destruction password off and on again (still the same).
    auth.submit_password(secret("login-password-1")).await;
    auth.disable_destruction_password(secret("login-password-1")).await.unwrap();
    assert!(!auth.destruction_enabled().await);
    auth.lock().await;
    auth.submit_password(secret("destroy-password-1")).await;
    assert_eq!(auth.state(), VaultState::Locked { error: Some(GENERIC_UNLOCK_ERROR.into()) }, "off: just a wrong password");
    auth.submit_password(secret("login-password-1")).await;
    auth.set_destruction_password(secret("login-password-1"), secret("destroy-password-1")).await.unwrap();
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
