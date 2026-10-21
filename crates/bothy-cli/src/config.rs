//! CLI configuration: paths, the database registry and private files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value as J, json};

use crate::fail::{CliError, CliResult};

/// Where the CLI keeps its files. `BOTHY_HOME` puts everything under one directory (tests,
/// containers); otherwise the platform's config and data directories are used.
#[derive(Clone, Debug)]
pub struct Paths {
    pub config: PathBuf,
    pub data: PathBuf,
}

impl Paths {
    pub fn new() -> CliResult<Self> {
        if let Some(home) = std::env::var_os("BOTHY_HOME") {
            let home = PathBuf::from(home);
            return Ok(Self {
                config: home.join("config"),
                data: home.join("data"),
            });
        }
        let (config, data) = platform_dirs()
            .ok_or_else(|| CliError::usage("cannot find the home directory; set BOTHY_HOME"))?;
        Ok(Self {
            config: config.join("bothy"),
            data: data.join("bothy"),
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.json")
    }

    pub fn client_file(&self, kind: &str) -> PathBuf {
        self.config.join(format!("client-{kind}.json"))
    }

    pub fn token_file(&self) -> PathBuf {
        self.config.join("token.json")
    }

    pub fn db_file(&self, name: &str) -> PathBuf {
        self.data.join(format!("{name}.db"))
    }
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// Base config and data directories: XDG on Linux and other Unixes, Application Support on
/// macOS, the roaming and local AppData folders on Windows.
fn platform_dirs() -> Option<(PathBuf, PathBuf)> {
    if cfg!(windows) {
        return Some((
            env_dir("APPDATA")?,
            env_dir("LOCALAPPDATA").or_else(|| env_dir("APPDATA"))?,
        ));
    }
    let home = env_dir("HOME")?;
    if cfg!(target_os = "macos") {
        let support = home.join("Library/Application Support");
        return Some((support.clone(), support));
    }
    Some((
        env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")),
        env_dir("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share")),
    ))
}

/// Writes a file readable only by the owner.
pub fn write_private(path: &Path, bytes: &[u8]) -> CliResult<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| CliError::io("cannot create config directory", e))?;
    }
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .map_err(|e| CliError::io("cannot write config file", e))?;
        f.write_all(bytes)
            .map_err(|e| CliError::io("cannot write config file", e))?;
        f.sync_all()
            .map_err(|e| CliError::io("cannot write config file", e))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| CliError::io("cannot write config file", e))
}

/// Where a database's remote files live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteSpec {
    /// Drive `appDataFolder`.
    AppData,
    /// Drive folder `Bothy/<name>`.
    Folder(String),
    /// A local or shared directory.
    Path(PathBuf),
}

impl RemoteSpec {
    /// Parses `appdata`, `folder`, `folder:<name>` or `path:<dir>`; a bare `folder` uses the
    /// database name.
    pub fn parse(s: &str, db_name: &str) -> CliResult<Self> {
        match s {
            "appdata" => Ok(Self::AppData),
            "folder" => Ok(Self::Folder(db_name.to_owned())),
            _ => {
                if let Some(name) = s
                    .strip_prefix("folder:")
                    .filter(|n| !n.is_empty() && !n.contains('/'))
                {
                    Ok(Self::Folder(name.to_owned()))
                } else if let Some(dir) = s.strip_prefix("path:").filter(|d| !d.is_empty()) {
                    let dir = std::path::absolute(dir).map_err(|e| CliError::io("bad path", e))?;
                    Ok(Self::Path(dir))
                } else {
                    Err(CliError::usage(format!(
                        "unknown remote {s:?}: use appdata, folder, folder:<name> or path:<dir>"
                    )))
                }
            }
        }
    }

    pub fn to_config(&self) -> String {
        match self {
            Self::AppData => "appdata".into(),
            Self::Folder(n) => format!("folder:{n}"),
            Self::Path(p) => format!("path:{}", p.display()),
        }
    }
}

/// A local database known to the CLI.
#[derive(Clone, Debug)]
pub struct DbEntry {
    pub remote: RemoteSpec,
    pub app_id: String,
}

/// `config.json`.
#[derive(Clone, Debug, Default)]
pub struct CliConfig {
    pub dbs: BTreeMap<String, DbEntry>,
}

impl CliConfig {
    pub fn load(paths: &Paths) -> CliResult<Self> {
        let file = paths.config_file();
        let bytes = match std::fs::read(&file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(CliError::io("cannot read config", e)),
        };
        let j: J = serde_json::from_slice(&bytes)
            .map_err(|_| CliError::usage(format!("{} is not valid JSON", file.display())))?;
        let mut dbs = BTreeMap::new();
        for (name, e) in j["dbs"].as_object().into_iter().flatten() {
            let remote = e["remote"]
                .as_str()
                .ok_or_else(|| CliError::usage("config: remote missing"))?;
            let app_id = e["app_id"]
                .as_str()
                .ok_or_else(|| CliError::usage("config: app_id missing"))?;
            dbs.insert(
                name.clone(),
                DbEntry {
                    remote: RemoteSpec::parse(remote, name)?,
                    app_id: app_id.to_owned(),
                },
            );
        }
        Ok(Self { dbs })
    }

    pub fn save(&self, paths: &Paths) -> CliResult<()> {
        let dbs: serde_json::Map<String, J> = self
            .dbs
            .iter()
            .map(|(n, e)| {
                (
                    n.clone(),
                    json!({"remote": e.remote.to_config(), "app_id": e.app_id}),
                )
            })
            .collect();
        let text =
            serde_json::to_vec_pretty(&json!({"dbs": dbs})).expect("JSON values always serialize");
        write_private(&paths.config_file(), &text)
    }
}

/// Database names are file names: `[A-Za-z0-9_-]{1,64}`.
pub fn check_db_name(name: &str) -> CliResult<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(CliError::usage(
            "database names use letters, digits, '_' and '-' (at most 64)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_specs() {
        assert_eq!(
            RemoteSpec::parse("appdata", "x").unwrap(),
            RemoteSpec::AppData
        );
        assert_eq!(
            RemoteSpec::parse("folder", "notes").unwrap(),
            RemoteSpec::Folder("notes".into())
        );
        assert_eq!(
            RemoteSpec::parse("folder:shared", "x").unwrap(),
            RemoteSpec::Folder("shared".into())
        );
        assert!(matches!(
            RemoteSpec::parse("path:/tmp/r", "x").unwrap(),
            RemoteSpec::Path(_)
        ));
        for bad in ["", "drive", "folder:", "folder:a/b", "path:"] {
            assert!(RemoteSpec::parse(bad, "x").is_err(), "{bad}");
        }
    }

    #[test]
    fn config_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("c"),
            data: dir.path().join("d"),
        };
        let mut c = CliConfig::default();
        c.dbs.insert(
            "notes".into(),
            DbEntry {
                remote: RemoteSpec::Folder("notes".into()),
                app_id: "dev.bothy.cli".into(),
            },
        );
        c.save(&paths).unwrap();
        let back = CliConfig::load(&paths).unwrap();
        assert_eq!(back.dbs["notes"].remote, RemoteSpec::Folder("notes".into()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(paths.config_file())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
