//! Native KWallet storage for KDE sessions.
//!
//! KWallet's DBus API returns integer status codes for mutating calls. Keep
//! those distinct from a missing entry so callers never authenticate with an
//! empty password while the wallet is locked or unavailable.

#[cfg(test)]
use zbus::blocking::connection::Builder as ConnectionBuilder;
use zbus::blocking::{Connection, Proxy};

const APPLICATION: &str = "weechat-rs";
const FOLDER: &str = APPLICATION;
const INTERFACE: &str = "org.kde.KWallet";
const SERVICES: [(&str, &str); 2] = [
    ("org.kde.kwalletd6", "/modules/kwalletd6"),
    ("org.kde.kwalletd5", "/modules/kwalletd5"),
];

struct KWallet {
    connection: Connection,
    service: &'static str,
    path: &'static str,
    wallet: String,
}

impl KWallet {
    fn session() -> Result<Self, String> {
        Self::from_connection(Connection::session().map_err(|error| error.to_string())?)
    }

    fn from_connection(connection: Connection) -> Result<Self, String> {
        let mut errors = Vec::with_capacity(SERVICES.len());
        for (service, path) in SERVICES {
            let proxy = match Self::proxy_for(&connection, service, path) {
                Ok(proxy) => proxy,
                Err(error) => {
                    errors.push(format!("{service}: {error}"));
                    continue;
                }
            };
            let wallet = match proxy.call::<_, _, String>("networkWallet", &()) {
                Ok(wallet) => wallet,
                Err(error) => {
                    errors.push(format!("{service}: {error}"));
                    continue;
                }
            };
            drop(proxy);
            return Ok(Self {
                connection,
                service,
                path,
                wallet,
            });
        }
        Err(format!("KWallet is unavailable ({})", errors.join("; ")))
    }

    fn proxy_for<'a>(
        connection: &'a Connection,
        service: &'static str,
        path: &'static str,
    ) -> Result<Proxy<'a>, String> {
        Proxy::new(connection, service, path, INTERFACE).map_err(|error| error.to_string())
    }

    fn proxy(&self) -> Result<Proxy<'_>, String> {
        Self::proxy_for(&self.connection, self.service, self.path)
    }

    fn open(&self) -> Result<i32, String> {
        let handle = self
            .proxy()?
            .call::<_, _, i32>("open", &(self.wallet.as_str(), 0_i64, APPLICATION))
            .map_err(|error| error.to_string())?;
        if handle < 0 {
            return Err(format!("KWallet open returned {handle}"));
        }
        Ok(handle)
    }

    fn has_folder(&self, handle: i32) -> Result<bool, String> {
        self.proxy()?
            .call("hasFolder", &(handle, FOLDER, APPLICATION))
            .map_err(|error| error.to_string())
    }

    fn create_folder(&self, handle: i32) -> Result<bool, String> {
        self.proxy()?
            .call("createFolder", &(handle, FOLDER, APPLICATION))
            .map_err(|error| error.to_string())
    }

    fn has_entry(&self, handle: i32, key: &str) -> Result<bool, String> {
        self.proxy()?
            .call("hasEntry", &(handle, FOLDER, key, APPLICATION))
            .map_err(|error| error.to_string())
    }

    fn read_password(&self, handle: i32, key: &str) -> Result<String, String> {
        self.proxy()?
            .call("readPassword", &(handle, FOLDER, key, APPLICATION))
            .map_err(|error| error.to_string())
    }

    fn write_password(&self, handle: i32, key: &str, password: &str) -> Result<i32, String> {
        self.proxy()?
            .call(
                "writePassword",
                &(handle, FOLDER, key, password, APPLICATION),
            )
            .map_err(|error| error.to_string())
    }

    fn remove_entry(&self, handle: i32, key: &str) -> Result<i32, String> {
        self.proxy()?
            .call("removeEntry", &(handle, FOLDER, key, APPLICATION))
            .map_err(|error| error.to_string())
    }

    fn load(&self, key: &str) -> Result<Option<String>, String> {
        let handle = self.open()?;
        if !self.has_entry(handle, key)? {
            return Ok(None);
        }
        self.read_password(handle, key).map(Some)
    }

    fn save(&self, key: &str, password: &str) -> Result<(), String> {
        let handle = self.open()?;
        if !self.has_folder(handle)? && !self.create_folder(handle)? {
            return Err("KWallet could not create the weechat-rs folder".to_string());
        }
        let result = self.write_password(handle, key, password)?;
        if result != 0 {
            return Err(format!("KWallet writePassword returned {result}"));
        }
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), String> {
        let handle = self.open()?;
        if !self.has_entry(handle, key)? {
            return Ok(());
        }
        let result = self.remove_entry(handle, key)?;
        if result != 0 {
            return Err(format!("KWallet removeEntry returned {result}"));
        }
        Ok(())
    }
}

pub(super) fn load(key: &str) -> Result<Option<String>, String> {
    KWallet::session()?.load(key)
}

pub(super) fn save(key: &str, password: &str) -> Result<(), String> {
    KWallet::session()?.save(key, password)
}

pub(super) fn delete(key: &str) -> Result<(), String> {
    KWallet::session()?.delete(key)
}

#[cfg(test)]
mod tests {
    use super::{ConnectionBuilder, KWallet, APPLICATION, FOLDER, SERVICES};
    use std::{
        collections::BTreeMap,
        fs,
        io::{BufRead, BufReader},
        path::PathBuf,
        process::{Child, Command, Stdio},
        sync::{
            atomic::{AtomicU64, Ordering},
            Mutex,
        },
    };
    use zbus::{blocking::Connection, interface};

    struct TestBus {
        _connection: Connection,
        daemon: Child,
        address: String,
        config_path: PathBuf,
    }

    impl TestBus {
        fn start(wallet: MockWallet) -> Self {
            Self::start_for_service(wallet, 0)
        }

        fn start_for_service(wallet: MockWallet, service_index: usize) -> Self {
            let config_path = isolated_bus_config();
            let mut daemon = Command::new("dbus-daemon")
                .args([
                    "--config-file",
                    config_path.to_str().expect("utf-8 temporary config path"),
                    "--nofork",
                    "--print-address=1",
                ])
                .stdout(Stdio::piped())
                .spawn()
                .expect("start isolated dbus-daemon");
            let mut address = String::new();
            BufReader::new(daemon.stdout.take().expect("daemon stdout"))
                .read_line(&mut address)
                .expect("read isolated dbus address");
            let address = address.trim().to_owned();
            let connection = ConnectionBuilder::address(address.as_str())
                .expect("parse isolated dbus address")
                .name(SERVICES[service_index].0)
                .expect("set KWallet test name")
                .serve_at(SERVICES[service_index].1, wallet)
                .expect("serve KWallet test object")
                .build()
                .expect("connect KWallet test service");
            Self {
                _connection: connection,
                daemon,
                address,
                config_path,
            }
        }

        fn client(&self) -> KWallet {
            let connection = ConnectionBuilder::address(self.address.as_str())
                .expect("parse client dbus address")
                .build()
                .expect("connect KWallet test client");
            KWallet::from_connection(connection).expect("connect mocked KWallet")
        }
    }

    impl Drop for TestBus {
        fn drop(&mut self) {
            let _ = self.daemon.kill();
            let _ = self.daemon.wait();
            let _ = fs::remove_file(&self.config_path);
        }
    }

    fn isolated_bus_config() -> PathBuf {
        static NEXT_TEST_BUS: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "weechat-rs-kwallet-test-{}-{}.conf",
            std::process::id(),
            NEXT_TEST_BUS.fetch_add(1, Ordering::Relaxed),
        ));
        fs::write(
            &path,
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/tmp</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*"/>
    <allow receive_sender="*"/>
    <allow own="*"/>
  </policy>
</busconfig>"#,
        )
        .expect("write isolated dbus configuration without activation directories");
        path
    }

    struct MockWallet {
        entries: Mutex<BTreeMap<String, String>>,
        folders: Mutex<bool>,
        open_result: i32,
        write_result: i32,
        remove_result: i32,
    }

    impl MockWallet {
        fn ready() -> Self {
            Self {
                entries: Mutex::new(BTreeMap::new()),
                folders: Mutex::new(false),
                open_result: 7,
                write_result: 0,
                remove_result: 0,
            }
        }
    }

    #[interface(name = "org.kde.KWallet")]
    impl MockWallet {
        #[zbus(name = "networkWallet")]
        fn network_wallet(&self) -> &str {
            "kdewallet"
        }

        #[zbus(name = "open")]
        fn open(&self, wallet: &str, _window_id: i64, application: &str) -> i32 {
            assert_eq!(wallet, "kdewallet");
            assert_eq!(application, APPLICATION);
            self.open_result
        }

        #[zbus(name = "hasFolder")]
        fn has_folder(&self, _handle: i32, folder: &str, application: &str) -> bool {
            assert_eq!(folder, FOLDER);
            assert_eq!(application, APPLICATION);
            *self.folders.lock().expect("folders lock")
        }

        #[zbus(name = "createFolder")]
        fn create_folder(&self, _handle: i32, folder: &str, application: &str) -> bool {
            assert_eq!(folder, FOLDER);
            assert_eq!(application, APPLICATION);
            *self.folders.lock().expect("folders lock") = true;
            true
        }

        #[zbus(name = "hasEntry")]
        fn has_entry(&self, _handle: i32, folder: &str, key: &str, application: &str) -> bool {
            assert_eq!(folder, FOLDER);
            assert_eq!(application, APPLICATION);
            self.entries.lock().expect("entries lock").contains_key(key)
        }

        #[zbus(name = "readPassword")]
        fn read_password(
            &self,
            _handle: i32,
            folder: &str,
            key: &str,
            application: &str,
        ) -> String {
            assert_eq!(folder, FOLDER);
            assert_eq!(application, APPLICATION);
            self.entries
                .lock()
                .expect("entries lock")
                .get(key)
                .cloned()
                .unwrap_or_default()
        }

        #[zbus(name = "writePassword")]
        fn write_password(
            &self,
            _handle: i32,
            folder: &str,
            key: &str,
            password: &str,
            application: &str,
        ) -> i32 {
            assert_eq!(folder, FOLDER);
            assert_eq!(application, APPLICATION);
            if self.write_result == 0 {
                self.entries
                    .lock()
                    .expect("entries lock")
                    .insert(key.to_owned(), password.to_owned());
            }
            self.write_result
        }

        #[zbus(name = "removeEntry")]
        fn remove_entry(&self, _handle: i32, folder: &str, key: &str, application: &str) -> i32 {
            assert_eq!(folder, FOLDER);
            assert_eq!(application, APPLICATION);
            if self.remove_result == 0 {
                self.entries.lock().expect("entries lock").remove(key);
            }
            self.remove_result
        }
    }

    #[test]
    fn mocked_kwallet_save_load_delete_and_missing() {
        let bus = TestBus::start(MockWallet::ready());
        let client = bus.client();
        assert_eq!(client.load("profile:9000").unwrap(), None);
        client.save("profile:9000", "saved-password").unwrap();
        assert_eq!(
            client.load("profile:9000").unwrap(),
            Some("saved-password".into())
        );
        client.delete("profile:9000").unwrap();
        assert_eq!(client.load("profile:9000").unwrap(), None);
    }

    #[test]
    fn mocked_kwallet_uses_the_kde5_service_and_path_when_kde6_is_absent() {
        let bus = TestBus::start_for_service(MockWallet::ready(), 1);
        assert_eq!(bus.client().load("profile:9000").unwrap(), None);
    }

    #[test]
    fn mocked_kwallet_surfaces_open_and_return_code_errors() {
        let mut unavailable = MockWallet::ready();
        unavailable.open_result = -1;
        let bus = TestBus::start(unavailable);
        assert!(bus
            .client()
            .load("profile:9000")
            .unwrap_err()
            .contains("open returned -1"));

        let mut write_failure = MockWallet::ready();
        write_failure.write_result = 23;
        let bus = TestBus::start(write_failure);
        assert!(bus
            .client()
            .save("profile:9000", "password")
            .unwrap_err()
            .contains("writePassword returned 23"));

        let mut remove_failure = MockWallet::ready();
        remove_failure.remove_result = 29;
        remove_failure
            .entries
            .get_mut()
            .expect("uncontended entries")
            .insert("profile:9000".into(), "password".into());
        let bus = TestBus::start(remove_failure);
        assert!(bus
            .client()
            .delete("profile:9000")
            .unwrap_err()
            .contains("removeEntry returned 29"));
    }
}
