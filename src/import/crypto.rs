//! Writing imported passwords into LiteBrowser's *own* WebView2 password store.
//!
//! WebView2 stores each saved password in its "Login Data" SQLite file, with the value encoded
//! in Chromium's `os_crypt` format: `v10` + 12-byte nonce + AES-256-GCM ciphertext. The AES key
//! lives DPAPI-protected in WebView2's own `Local State`.
//!
//! This module only ever reads LiteBrowser's own WebView2 key and encrypts into LiteBrowser's own
//! store. It does not read any other browser's credentials — imported passwords come from a CSV
//! the user exported themselves (see `csv.rs`).

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use std::path::Path;

use super::Unprotect;

pub type Key = [u8; 32];

/// Reads and unwraps the AES key from LiteBrowser's own WebView2 `Local State` file.
pub fn key_from_local_state(local_state: &Path, unprotect: Unprotect) -> Option<Key> {
    let text = std::fs::read_to_string(local_state).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let encoded = json.get("os_crypt")?.get("encrypted_key")?.as_str()?;
    let blob = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
    let blob = blob.strip_prefix(b"DPAPI")?;
    unprotect(blob)?.try_into().ok()
}

/// Encodes a password the way WebView2 expects (`v10` + nonce + AES-GCM) so it autofills.
pub fn encrypt_v10(key: &Key, plaintext: &[u8]) -> Option<Vec<u8>> {
    let mut nonce = [0u8; 12];
    getrandom::getrandom(&mut nonce).ok()?;
    let ciphertext = Aes256Gcm::new_from_slice(key).ok()?.encrypt(Nonce::from_slice(&nonce), plaintext).ok()?;
    let mut out = b"v10".to_vec();
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Some(out)
}

/// Round-trips a value through the same AES-GCM key (used by tests).
#[cfg(test)]
pub fn decrypt_v10(key: &Key, value: &[u8]) -> Option<Vec<u8>> {
    let rest = value.strip_prefix(b"v10").or_else(|| value.strip_prefix(b"v11"))?;
    if rest.len() < 12 + 16 {
        return None;
    }
    let (nonce, ciphertext) = rest.split_at(12);
    Aes256Gcm::new_from_slice(key).ok()?.decrypt(Nonce::from_slice(nonce), ciphertext).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v10_roundtrip_into_own_store() {
        let key = [3u8; 32];
        let enc = encrypt_v10(&key, b"hunter2").unwrap();
        assert!(enc.starts_with(b"v10"));
        assert_eq!(decrypt_v10(&key, &enc).as_deref(), Some(b"hunter2".as_slice()));
        assert_eq!(decrypt_v10(&[4u8; 32], &enc), None);
    }

    #[test]
    fn reads_own_webview2_key() {
        let dir = super::super::test_dir("localstate");
        let path = dir.join("Local State");
        let mut blob = b"DPAPI".to_vec();
        blob.extend([9u8; 32]);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&blob);
        std::fs::write(&path, format!(r#"{{"os_crypt":{{"encrypted_key":"{encoded}"}}}}"#)).unwrap();
        let identity = |b: &[u8]| Some(b.to_vec());
        assert_eq!(key_from_local_state(&path, &identity), Some([9u8; 32]));
        let none = |_: &[u8]| None;
        assert_eq!(key_from_local_state(&path, &none), None);
    }
}
