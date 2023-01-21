//! Application state.

use crate::{Map, SharedString};
use std::{
    borrow::Cow,
    env, fs,
    net::{IpAddr, SocketAddr},
    sync::LazyLock,
};
use toml::value::{Table, Value};

/// Application state.
#[derive(Debug, Clone)]
pub struct State {
    /// Environment.
    env: SharedString,
    /// Configuration.
    config: Table,
    /// Associated data.
    data: Map,
}

impl State {
    /// Creates a new instance.
    #[inline]
    pub fn new(env: impl Into<SharedString>) -> Self {
        Self {
            env: env.into(),
            config: Table::new(),
            data: Map::new(),
        }
    }

    /// Loads the config file according to the specific env.
    pub fn load_config(&mut self) {
        let env = self.env.as_ref();
        let project_dir = env::current_dir()
            .expect("the project directory does not exist or permissions are insufficient");
        let config_file = project_dir.join(format!("./config/config.{env}.toml"));
        let config: Value = fs::read_to_string(&config_file)
            .unwrap_or_else(|err| {
                let config_file = config_file.to_string_lossy();
                panic!("failed to read the config file `{config_file}`: {err}");
            })
            .parse()
            .expect("failed to parse toml value");
        match config {
            Value::Table(table) => self.config = table,
            _ => panic!("toml config file should be a table"),
        }
    }

    /// Set the state data.
    #[inline]
    pub fn set_data(&mut self, data: Map) {
        self.data = data;
    }

    /// Returns the env as `&str`.
    #[inline]
    pub fn env(&self) -> &str {
        self.env.as_ref()
    }

    /// Returns a reference to the config.
    #[inline]
    pub fn config(&self) -> &Table {
        &self.config
    }

    /// Returns a reference to the data.
    #[inline]
    pub fn data(&self) -> &Map {
        &self.data
    }

    /// Returns a mutable reference to the data.
    #[inline]
    pub fn data_mut(&mut self) -> &mut Map {
        &mut self.data
    }

    /// Returns a list of listeners as `Vec<SocketAddr>`.
    pub fn listeners(&self) -> Vec<SocketAddr> {
        let config = self.config();
        let mut listeners = Vec::new();

        // Main server.
        let main = config
            .get("main")
            .expect("the `main` field should be specified")
            .as_table()
            .expect("the `main` field should be a table");
        let main_host = main
            .get("host")
            .expect("the `main.host` field should be specified")
            .as_str()
            .and_then(|s| s.parse::<IpAddr>().ok())
            .expect("the `main.host` field should be an IP address");
        let main_port = main
            .get("port")
            .expect("the `main.port` field should be specified")
            .as_integer()
            .and_then(|i| u16::try_from(i).ok())
            .expect("the `main.port` field should be an integer");
        listeners.push((main_host, main_port).into());

        // Standbys.
        let standbys = config
            .get("standby")
            .expect("the `standby` field should be specified")
            .as_array()
            .expect("the `standby` field should be an array of tables");
        for standby in standbys {
            if standby.is_table() {
                let standby = standby
                    .as_table()
                    .expect("the `standby` field should be a table");
                let standby_host = standby
                    .get("host")
                    .expect("the `standby.host` field should be specified")
                    .as_str()
                    .and_then(|s| s.parse::<IpAddr>().ok())
                    .expect("the `standby.host` field should be a str");
                let standby_port = standby
                    .get("port")
                    .expect("the `standby.port` field should be specified")
                    .as_integer()
                    .and_then(|i| u16::try_from(i).ok())
                    .expect("the `standby.port` field should be an integer");
                listeners.push((standby_host, standby_port).into());
            }
        }

        listeners
    }

    /// Returns a reference to the shared state.
    #[inline]
    pub(crate) fn shared() -> &'static State {
        LazyLock::force(&SHARED_STATE)
    }
}

impl Default for State {
    fn default() -> Self {
        SHARED_STATE.clone()
    }
}

/// Shared application state.
pub(crate) static SHARED_STATE: LazyLock<State> = LazyLock::new(|| {
    let mut app_env = Cow::from("dev");
    for arg in env::args() {
        if let Some(value) = arg.strip_prefix("--env=") {
            app_env = value.to_owned().into();
        }
    }

    let mut state = State::new(app_env);
    state.load_config();
    state
});
