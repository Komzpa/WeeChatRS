use keyring::Entry;

const SERVICE: &str = "weechat-rs";

/// Result of reading a credential from the platform secure store.
///
/// `NoEntry` is a normal passwordless-profile state. All other failures are
/// kept distinct so startup can wait for Secret Service (or its platform
/// equivalent) instead of accidentally attempting anonymous authentication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoadResult {
    Found(String),
    Missing,
    Unavailable(String),
}

fn load_entry(key: &str) -> LoadResult {
    match Entry::new(SERVICE, key) {
        Ok(entry) => match entry.get_password() {
            Ok(password) => LoadResult::Found(password),
            Err(keyring::Error::NoEntry) => LoadResult::Missing,
            Err(error) => LoadResult::Unavailable(error.to_string()),
        },
        Err(keyring::Error::NoEntry) => LoadResult::Missing,
        Err(error) => LoadResult::Unavailable(error.to_string()),
    }
}

// On Linux, keyring uses zbus::blocking which calls block_on internally.
// The egui update loop runs inside a tokio runtime (CachedParkThread::block_on
// from #[tokio::main]), so any attempt to call block_on from it will panic.
// Spawning a dedicated OS thread gives zbus a clean stack with no runtime context.
// We block on the result via a channel — these calls are infrequent (button clicks only).
#[cfg(target_os = "linux")]
fn run_keyring<F, R>(f: F) -> Result<R, String>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv().map_err(|_| "keyring helper thread panicked or was dropped".to_string())
}

#[allow(dead_code)]
fn user_key(host: &str, port: &str) -> String {
    format!("{}:{}", host, port)
}

#[allow(dead_code)]
pub fn save(host: &str, port: &str, password: &str) -> Result<(), String> {
    let key = user_key(host, port);
    let password = password.to_string();
    #[cfg(target_os = "linux")]
    return run_keyring(move || {
        Entry::new(SERVICE, &key)
            .and_then(|e| e.set_password(&password))
            .map_err(|e| e.to_string())
    }).and_then(|r| r);
    #[cfg(not(target_os = "linux"))]
    Entry::new(SERVICE, &key)
        .and_then(|e| e.set_password(&password))
        .map_err(|e| e.to_string())
}

#[allow(dead_code)]
pub fn load(host: &str, port: &str) -> Option<String> {
    match load_status(host, port) {
        LoadResult::Found(password) => Some(password),
        LoadResult::Missing | LoadResult::Unavailable(_) => None,
    }
}

pub(crate) fn load_status(host: &str, port: &str) -> LoadResult {
    load_by_key_status(&user_key(host, port))
}

#[allow(dead_code)]
pub fn delete(host: &str, port: &str) -> Result<(), String> {
    let key = user_key(host, port);
    #[cfg(target_os = "linux")]
    return run_keyring(move || {
        Entry::new(SERVICE, &key)
            .and_then(|e| e.delete_credential())
            .map_err(|e| e.to_string())
    }).and_then(|r| r);
    #[cfg(not(target_os = "linux"))]
    Entry::new(SERVICE, &key)
        .and_then(|e| e.delete_credential())
        .map_err(|e| e.to_string())
}

pub fn save_by_key(key: &str, password: &str) -> Result<(), String> {
    let key = key.to_string();
    let password = password.to_string();
    #[cfg(target_os = "linux")]
    return run_keyring(move || {
        Entry::new(SERVICE, &key)
            .and_then(|e| e.set_password(&password))
            .map_err(|e| e.to_string())
    }).and_then(|r| r);
    #[cfg(not(target_os = "linux"))]
    Entry::new(SERVICE, &key)
        .and_then(|e| e.set_password(&password))
        .map_err(|e| e.to_string())
}

pub fn load_by_key(key: &str) -> Option<String> {
    match load_by_key_status(key) {
        LoadResult::Found(password) => Some(password),
        LoadResult::Missing | LoadResult::Unavailable(_) => None,
    }
}

pub(crate) fn load_by_key_status(key: &str) -> LoadResult {
    let key = key.to_string();
    #[cfg(target_os = "linux")]
    return run_keyring(move || load_entry(&key))
        .unwrap_or_else(|error| LoadResult::Unavailable(error));
    #[cfg(not(target_os = "linux"))]
    load_entry(&key)
}
