use keyring::Entry;

#[cfg(target_os = "linux")]
mod kwallet;

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

#[cfg(target_os = "linux")]
fn is_kde_desktop(current_desktop: Option<&str>) -> bool {
    current_desktop
        .into_iter()
        .flat_map(|desktop| desktop.split(':'))
        .any(|desktop| desktop.eq_ignore_ascii_case("KDE"))
}

#[cfg(target_os = "linux")]
fn use_kwallet() -> bool {
    is_kde_desktop(std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref())
}

#[cfg(target_os = "linux")]
fn delete_legacy_entry(key: &str) -> Result<(), String> {
    match Entry::new(SERVICE, key).and_then(|entry| entry.delete_credential()) {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(target_os = "linux")]
fn load_kwallet_entry(key: &str) -> LoadResult {
    match kwallet::load(key) {
        Ok(Some(password)) => LoadResult::Found(password),
        Ok(None) => match load_entry(key) {
            LoadResult::Found(password) => {
                if let Err(error) = kwallet::save(key, &password) {
                    return LoadResult::Unavailable(format!(
                        "KWallet could not import the existing Secret Service credential: {error}"
                    ));
                }
                match delete_legacy_entry(key) {
                    Ok(()) => LoadResult::Found(password),
                    Err(error) => LoadResult::Unavailable(format!(
                        "KWallet imported the existing credential, but could not remove the legacy Secret Service copy: {error}"
                    )),
                }
            }
            LoadResult::Missing => LoadResult::Missing,
            LoadResult::Unavailable(error) => LoadResult::Unavailable(format!(
                "KWallet has no entry and the legacy Secret Service credential could not be checked: {error}"
            )),
        },
        Err(error) => LoadResult::Unavailable(error),
    }
}

#[cfg(target_os = "linux")]
fn save_kwallet_entry(key: &str, password: &str) -> Result<(), String> {
    kwallet::save(key, password)
}

#[cfg(target_os = "linux")]
fn delete_kwallet_entry(key: &str) -> Result<(), String> {
    // Remove the migration source first. If it is locked, leave the native
    // value intact: otherwise a later native miss could resurrect the legacy
    // credential after a user asked to delete it.
    delete_legacy_entry(key).map_err(|error| {
        format!(
            "KWallet kept the credential because the legacy Secret Service copy could not be removed: {error}"
        )
    })?;
    kwallet::delete(key)
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
    rx.recv()
        .map_err(|_| "keyring helper thread panicked or was dropped".to_string())
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
        if use_kwallet() {
            return save_kwallet_entry(&key, &password);
        }
        Entry::new(SERVICE, &key)
            .and_then(|e| e.set_password(&password))
            .map_err(|e| e.to_string())
    })
    .and_then(|r| r);
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
        if use_kwallet() {
            return delete_kwallet_entry(&key);
        }
        Entry::new(SERVICE, &key)
            .and_then(|e| e.delete_credential())
            .map_err(|e| e.to_string())
    })
    .and_then(|r| r);
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
        if use_kwallet() {
            return save_kwallet_entry(&key, &password);
        }
        Entry::new(SERVICE, &key)
            .and_then(|e| e.set_password(&password))
            .map_err(|e| e.to_string())
    })
    .and_then(|r| r);
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
    return run_keyring(move || {
        if use_kwallet() {
            load_kwallet_entry(&key)
        } else {
            load_entry(&key)
        }
    })
    .unwrap_or_else(|error| LoadResult::Unavailable(error));
    #[cfg(not(target_os = "linux"))]
    load_entry(&key)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::is_kde_desktop;

    #[test]
    fn kde_desktop_detection_uses_xdg_tokens() {
        assert!(is_kde_desktop(Some("KDE")));
        assert!(is_kde_desktop(Some("ubuntu:KDE:wayland")));
        assert!(is_kde_desktop(Some("kde:GNOME")));
        assert!(!is_kde_desktop(Some("GNOME:wayland")));
        assert!(!is_kde_desktop(None));
    }
}
