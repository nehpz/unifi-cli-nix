//! Sessions are scoped to a canonical controller origin and config profile.
//! No plaintext fallback is used when the OS credential store is unavailable.

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::api::{
    ApiError,
    session::{LOGIN_HINT, SessionSecret, controller_origin},
};

pub trait SessionStore {
    fn load(&self) -> Result<Option<SessionSecret>, ApiError>;
    fn save(&self, secret: &SessionSecret) -> Result<(), ApiError>;
    fn delete(&self) -> Result<bool, ApiError>;
}

pub struct OsSessionStore {
    account: String,
}

impl OsSessionStore {
    pub fn new(host: &str, profile: Option<&str>) -> Result<Self, ApiError> {
        Ok(Self {
            account: session_account(host, profile)?,
        })
    }

    fn entry(&self) -> Result<keyring::Entry, ApiError> {
        keyring::Entry::new("unifi-cli/session/v1", &self.account).map_err(store_error)
    }
}

fn session_account(host: &str, profile: Option<&str>) -> Result<String, ApiError> {
    let origin = controller_origin(host)?;
    // Serialize the tuple so None, an empty profile, and separator-containing
    // profile names cannot alias each other. Fixed length fits native backends.
    let scope = serde_json::to_vec(&(origin, profile)).expect("scope is serializable");
    Ok(format!("{:x}", Sha256::digest(scope)))
}

impl SessionStore for OsSessionStore {
    fn load(&self) -> Result<Option<SessionSecret>, ApiError> {
        match self.entry()?.get_secret() {
            Ok(value) => {
                let value = Zeroizing::new(value);
                serde_json::from_slice(&value).map(Some).map_err(|_| {
                    ApiError::SessionAuth(format!("Stored session is invalid. {LOGIN_HINT}"))
                })
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(store_error(e)),
        }
    }

    fn save(&self, secret: &SessionSecret) -> Result<(), ApiError> {
        let value = Zeroizing::new(
            serde_json::to_vec(secret)
                .map_err(|_| ApiError::Other("Could not serialize session.".into()))?,
        );
        self.entry()?.set_secret(&value).map_err(store_error)
    }

    fn delete(&self) -> Result<bool, ApiError> {
        match self.entry()?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(store_error(e)),
        }
    }
}

/// Prefer the saved session to incidental Protect credentials. An explicit
/// ephemeral login skips the OS store entirely, including access prompts.
pub fn saved_session_for_watch(
    store: &dyn SessionStore,
    ephemeral: bool,
    credentials_complete: bool,
) -> Result<Option<SessionSecret>, ApiError> {
    if ephemeral {
        return Ok(None);
    }
    if let Some(secret) = store.load()? {
        return Ok(Some(secret));
    }
    if credentials_complete {
        return Ok(None);
    }
    Err(ApiError::SessionAuth(format!(
        "No saved session. {LOGIN_HINT}"
    )))
}

fn store_error(_: keyring::Error) -> ApiError {
    ApiError::Other("OS credential store unavailable or access denied. Unlock macOS Keychain, Windows Credential Manager, or the Linux Secret Service. No session was written to a plaintext file. For an ephemeral session, supply UNIFI_USERNAME and UNIFI_PASSWORD with `events watch --login`.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_is_canonical_and_isolated() {
        let base = session_account("https://controller.example", None).unwrap();
        assert_eq!(
            base,
            session_account("https://CONTROLLER.example:443/", None).unwrap()
        );
        for (host, profile) in [
            ("https://controller.example", Some("home")),
            ("https://controller.example", Some("")),
            ("https://other.example", None),
            ("https://controller.example:8443", None),
        ] {
            assert_ne!(base, session_account(host, profile).unwrap());
        }
        assert_eq!(base.len(), 64);
    }

    #[test]
    fn saved_session_precedes_partial_or_complete_credentials() {
        struct FakeStore {
            saved: bool,
            calls: std::cell::Cell<usize>,
        }
        impl SessionStore for FakeStore {
            fn load(&self) -> Result<Option<SessionSecret>, ApiError> {
                self.calls.set(self.calls.get() + 1);
                Ok(self.saved.then(|| serde_json::from_value(serde_json::json!({"origin":"https://controller.example", "token":"fake", "csrf":null})).unwrap()))
            }
            fn save(&self, _: &SessionSecret) -> Result<(), ApiError> {
                unreachable!()
            }
            fn delete(&self) -> Result<bool, ApiError> {
                unreachable!()
            }
        }
        for saved in [false, true] {
            for username in [false, true] {
                for password in [false, true] {
                    let store = FakeStore {
                        saved,
                        calls: std::cell::Cell::new(0),
                    };
                    let result = saved_session_for_watch(&store, false, username && password);
                    assert_eq!(result.is_ok(), saved || (username && password));
                    if let Ok(value) = result {
                        assert_eq!(value.is_some(), saved);
                    }
                    assert_eq!(store.calls.get(), 1);
                    assert!(
                        saved_session_for_watch(&store, true, username && password)
                            .unwrap()
                            .is_none()
                    );
                    assert_eq!(
                        store.calls.get(),
                        1,
                        "ephemeral auth must skip the OS store"
                    );
                }
            }
        }
    }
}
