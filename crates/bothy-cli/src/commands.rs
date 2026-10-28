//! Command handlers.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;

use bothy::codec::keys::{KeyKind, LogicalKey};
use bothy::codec::manifest::{DeviceInfo, Manifest};
use bothy::merge::DocState;
use bothy::remote::drive::DriveRemote;
use bothy::remote::{FolderRemote, RemoteStore};
use bothy::{
    AutoSync, Config, Database, DbId, DeviceId, ErrorKind, ListOptions, Origin, Stamp, SyncReport,
    Value,
};
use serde_json::{Value as J, json};

use crate::cli::{
    Auth, Bench, Command, Devices, Init, Inspect, Join, P2p, Setup, SetupWhat, Snapshot,
};
use crate::config::{CliConfig, DbEntry, Paths, RemoteSpec, check_db_name, write_private};
use crate::fail::{CliError, CliResult};
use crate::oauth::{self, SCOPE_APPDATA, SCOPE_FILE};
use crate::secrets::{self, Ask};
use crate::setup;
use crate::tokens;

pub struct Ctx {
    pub paths: Paths,
    pub json: bool,
    pub db: String,
    pub password_stdin: bool,
}

impl Ctx {
    /// Prints a result: compact JSON with `--json`, otherwise the human text.
    fn out(&self, value: J, human: impl FnOnce() -> String) {
        if self.json {
            println!("{value}");
        } else {
            let text = human();
            if !text.is_empty() {
                println!("{text}");
            }
        }
    }
}

/// UTC time `YYYY-MM-DD HH:MM:SS` from milliseconds since the epoch.
pub fn fmt_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `--since`: a duration back from `now_ms` (`90s`, `30m`, `2h`, `7d`) or a UTC date or time
/// (`2026-10-21`, `2026-10-21T14:30`, `2026-10-21 14:30:05`), as milliseconds since the epoch.
pub fn parse_since(s: &str, now_ms: u64) -> CliResult<u64> {
    let bad = || {
        CliError::usage(format!(
            "--since: expected a duration like 2h or a UTC date like 2026-10-21, got {s:?}"
        ))
    };
    let s = s.trim();
    if !s.contains('-') {
        let (n, unit) = s.split_at(s.len().saturating_sub(1));
        let n: u64 = n.parse().map_err(|_| bad())?;
        let unit_ms = match unit {
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            _ => return Err(bad()),
        };
        return Ok(now_ms.saturating_sub(n.saturating_mul(unit_ms)));
    }
    let s = s.strip_suffix('Z').unwrap_or(s);
    let (date, time) = s.split_once(['T', ' ']).unwrap_or((s, "00:00"));
    let num = |p: Option<&str>, lo: i64, hi: i64| {
        p.and_then(|p| p.parse::<i64>().ok())
            .filter(|v| (lo..=hi).contains(v))
            .ok_or_else(bad)
    };
    let mut d = date.split('-');
    let (y, m, day) = (
        num(d.next(), 1970, 9999)?,
        num(d.next(), 1, 12)?,
        num(d.next(), 1, 31)?,
    );
    let mut t = time.split(':');
    let (hh, mm) = (num(t.next(), 0, 23)?, num(t.next(), 0, 59)?);
    let ss = t.next().map_or(Ok(0), |v| num(Some(v), 0, 59))?;
    if d.next().is_some() || t.next().is_some() {
        return Err(bad());
    }
    // Days since 1970-01-01 from a civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Ok(((days * 86_400 + hh * 3600 + mm * 60 + ss) * 1000) as u64)
}

fn encryption_note(encrypted: bool, kept: Option<&str>) -> String {
    match (encrypted, kept) {
        (false, _) => String::new(),
        (true, Some(w)) => format!("\nencrypted; the data key is kept in {w}"),
        (true, None) => {
            "\nencrypted; syncing asks for the password (`bothy unlock --remember` keeps the key)"
                .into()
        }
    }
}

fn conflict_json(c: &bothy::store::Conflict) -> J {
    json!({
        "time": fmt_ms(c.at_ms),
        "collection": c.collection.as_str(),
        "id": c.doc_id.as_str(),
        "field": c.field.to_string(),
        "winner": stamp_json(&c.winner),
        "loser": stamp_json(&c.loser),
        "loser_value": c.loser_value.as_ref().map_or(J::Null, Value::to_json),
    })
}

fn stamp_json(s: &Stamp) -> J {
    json!({"hlc": s.hlc.to_hex(), "device": s.device.to_hex(), "time": fmt_ms(s.hlc.physical_ms())})
}

fn host_name() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn entry(ctx: &Ctx, config: &CliConfig) -> CliResult<DbEntry> {
    config.dbs.get(&ctx.db).cloned().ok_or_else(|| {
        CliError::usage(format!(
            "no local database {:?}; create one with `bothy init` or `bothy join` (use --db to pick another)",
            ctx.db
        ))
    })
}

fn remote_for(ctx: &Ctx, spec: &RemoteSpec) -> CliResult<Arc<dyn RemoteStore>> {
    Ok(match spec {
        RemoteSpec::Path(dir) => Arc::new(FolderRemote::new(dir)?),
        RemoteSpec::AppData | RemoteSpec::Folder(_) => {
            let grant = tokens::load(&ctx.paths).ok_or_else(|| {
                CliError::auth("not signed in; run `bothy auth login` (or `auth login --device`)")
            })?;
            if *spec == RemoteSpec::AppData && !grant.has(SCOPE_APPDATA) {
                return Err(CliError::auth(
                    "this database uses the hidden app folder (appdata), but the current sign-in cannot reach it.\n\
                     Google does not allow the drive.appdata scope in device sign-in, so headless machines need a\n\
                     folder database: create it with `bothy init --remote folder` on a machine with a browser.",
                ));
            }
            if !grant.has(SCOPE_FILE) && matches!(spec, RemoteSpec::Folder(_)) {
                return Err(CliError::auth(
                    "the current sign-in lacks drive.file; run `bothy auth login` again",
                ));
            }
            let client = tokens::client(&ctx.paths, &grant.client)?;
            let provider = Arc::new(tokens::CliTokens::new(client, &grant)?);
            Arc::new(match spec {
                RemoteSpec::AppData => DriveRemote::appdata(provider)?,
                RemoteSpec::Folder(name) => DriveRemote::folder(provider, name)?,
                RemoteSpec::Path(_) => unreachable!("handled above"),
            })
        }
    })
}

/// Opens the database; an encrypted one is unlocked with its remembered key, if there is one.
async fn open_db(ctx: &Ctx, entry: &DbEntry) -> CliResult<Database> {
    let db = open_db_with(ctx, entry, |c| c).await?;
    if let Some(db_id) = db.db_id().await?
        && db.is_locked().await?
        && let Some(key) = secrets::remembered(&ctx.paths, db_id)
        && db.unlock_with_key(key).await.is_err()
    {
        eprintln!(
            "warning: the remembered key does not unlock this database; run `bothy unlock --remember` again"
        );
    }
    Ok(db)
}

async fn open_db_with(
    ctx: &Ctx,
    entry: &DbEntry,
    tweak: impl FnOnce(Config) -> Config,
) -> CliResult<Database> {
    let remote = remote_for(ctx, &entry.remote)?;
    std::fs::create_dir_all(&ctx.paths.data)
        .map_err(|e| CliError::io("cannot create data directory", e))?;
    let mut config = Config::new(ctx.paths.db_file(&ctx.db), &entry.app_id);
    if let Some(h) = host_name() {
        config = config.device_name(&h);
    }
    Ok(Database::open(tweak(config), remote).await?)
}

/// For commands that need the data key: unlocks a locked database with its password.
async fn unlocked(ctx: &Ctx, db: &Database) -> CliResult<()> {
    if db.is_locked().await? {
        let password = secrets::read(ctx.password_stdin, Ask::Current)?;
        db.unlock(&password).await?;
    }
    Ok(())
}

async fn encrypted_db_id(db: &Database) -> CliResult<DbId> {
    let db_id = db
        .db_id()
        .await?
        .ok_or_else(|| CliError::usage("database is not attached"))?;
    if !db.is_encrypted().await? {
        return Err(CliError::usage("this database is not encrypted"));
    }
    Ok(db_id)
}

/// Opens the database with direct channels as asked by the P2P flags.
async fn open_p2p(ctx: &Ctx, p2p: &P2p, inline: bool) -> CliResult<Database> {
    if !p2p.p2p {
        return open(ctx).await;
    }
    let config = CliConfig::load(&ctx.paths)?;
    let entry = entry(ctx, &config)?;
    let mut cfg = bothy::p2p::P2pConfig::new().inline_segments(inline);
    if p2p.no_stun {
        cfg = cfg.stun_servers(Vec::new());
    } else if !p2p.stun.is_empty() {
        cfg = cfg.stun_servers(p2p.stun.clone());
    }
    if let Some(ip) = p2p.host {
        cfg = cfg.host_address(ip);
    }
    let db = open_db_with(ctx, &entry, |c| c.p2p(cfg)).await?;
    if let Some(db_id) = db.db_id().await?
        && db.is_locked().await?
        && let Some(key) = secrets::remembered(&ctx.paths, db_id)
    {
        let _ = db.unlock_with_key(key).await;
    }
    Ok(db)
}

const BENCH: &str = "bench";

struct BenchOpts {
    count: u32,
    interval: std::time::Duration,
    timeout: std::time::Duration,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Waits until `db` has an open channel, up to `limit`.
async fn wait_connected(db: &Database, limit: std::time::Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < limit {
        if db.p2p_status().connected() > 0 {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    false
}

fn summary(mut ms: Vec<u64>, count: u32, connect_ms: Option<u64>) -> J {
    ms.sort_unstable();
    let pick = |q: usize| {
        ms.get((ms.len() * q / 100).min(ms.len().saturating_sub(1)))
            .copied()
    };
    json!({
        "count": count,
        "arrived": ms.len(),
        "p50_ms": pick(50),
        "p95_ms": pick(95),
        "max_ms": ms.last(),
        "connect_ms": connect_ms,
        "samples_ms": ms,
    })
}

/// Two local databases of the same remote database, in this process.
async fn bench_pair(a: &Database, b: &Database, p2p: bool, o: &BenchOpts) -> CliResult<J> {
    let ha = a.start_auto_sync(AutoSync::default());
    let hb = b.start_auto_sync(AutoSync::default());
    let started = std::time::Instant::now();
    let connect_ms = if p2p {
        if !(wait_connected(a, std::time::Duration::from_secs(180)).await
            && wait_connected(b, std::time::Duration::from_secs(30)).await)
        {
            ha.stop().await;
            hb.stop().await;
            return Err(CliError::usage("no direct channel within 3 minutes"));
        }
        Some(started.elapsed().as_millis() as u64)
    } else {
        None
    };
    let bench = a.collection(BENCH)?;
    let run = now_ms();
    let mut got = Vec::new();
    for i in 0..o.count {
        let id = format!("p{run}-{i:04}");
        let mut events = b.subscribe();
        let sent = std::time::Instant::now();
        bench.put(&id, json!({"sent_ms": now_ms()})).await?;
        let arrived = tokio::time::timeout(o.timeout, async {
            loop {
                match events.recv().await {
                    Ok(e) if e.origin == Origin::Remote && e.doc_id.as_str() == id => return,
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => std::future::pending::<()>().await,
                }
            }
        })
        .await;
        if arrived.is_ok() {
            got.push(sent.elapsed().as_millis() as u64);
        }
        tokio::time::sleep(o.interval).await;
    }
    ha.stop().await;
    hb.stop().await;
    Ok(summary(got, o.count, connect_ms))
}

/// Writes to a device running `bench echo` and times its acknowledgements (one way, from the
/// other device's clock, and round trip).
async fn bench_remote(a: &Database, p2p: bool, o: &BenchOpts) -> CliResult<J> {
    let ha = a.start_auto_sync(AutoSync::default());
    let started = std::time::Instant::now();
    let connect_ms = if p2p {
        if !wait_connected(a, std::time::Duration::from_secs(180)).await {
            ha.stop().await;
            return Err(CliError::usage(
                "no direct channel within 3 minutes; is `bench echo` running?",
            ));
        }
        Some(started.elapsed().as_millis() as u64)
    } else {
        None
    };
    let bench = a.collection(BENCH)?;
    let run = now_ms();
    let (mut one_way, mut round) = (Vec::new(), Vec::new());
    for i in 0..o.count {
        let id = format!("p{run}-{i:04}");
        let ack = format!("a{run}-{i:04}");
        let mut events = a.subscribe();
        let sent = std::time::Instant::now();
        bench.put(&id, json!({"sent_ms": now_ms()})).await?;
        let arrived = tokio::time::timeout(o.timeout, async {
            loop {
                match events.recv().await {
                    Ok(e) if e.origin == Origin::Remote && e.doc_id.as_str() == ack => return,
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => std::future::pending::<()>().await,
                }
            }
        })
        .await;
        if arrived.is_ok() {
            round.push(sent.elapsed().as_millis() as u64);
            if let Some(doc) = bench.get(&ack).await? {
                let d = doc.to_json();
                if let (Some(s), Some(r)) = (d["sent_ms"].as_u64(), d["recv_ms"].as_u64()) {
                    one_way.push(r.saturating_sub(s));
                }
            }
        }
        tokio::time::sleep(o.interval).await;
    }
    ha.stop().await;
    let mut out = summary(one_way, o.count, connect_ms);
    out["round_trip"] = summary(round, o.count, None);
    Ok(out)
}

/// Asks a device running `bench echo` (or the demo app's bench mode) for `count` writes and
/// times how long each takes to arrive here: `now - sent_ms`, so both clocks must agree.
async fn bench_reverse(a: &Database, p2p: bool, o: &BenchOpts) -> CliResult<J> {
    let ha = a.start_auto_sync(AutoSync::default());
    let started = std::time::Instant::now();
    let connect_ms = if p2p {
        if !wait_connected(a, std::time::Duration::from_secs(180)).await {
            ha.stop().await;
            return Err(CliError::usage(
                "no direct channel within 3 minutes; is `bench echo` running?",
            ));
        }
        Some(started.elapsed().as_millis() as u64)
    } else {
        None
    };
    let bench = a.collection(BENCH)?;
    let run = now_ms();
    let prefix = format!("q{run}-");
    let mut events = a.subscribe();
    let interval_ms = o.interval.as_millis() as u64;
    bench
        .put(
            &format!("s{run}"),
            json!({"count": o.count, "interval_ms": interval_ms}),
        )
        .await?;
    let mut got: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let deadline = tokio::time::Instant::now() + o.timeout + o.interval * o.count;
    while got.len() < o.count as usize {
        let next = tokio::time::timeout_at(deadline, events.recv()).await;
        let e = match next {
            Ok(Ok(e)) => e,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => break,
        };
        let id = e.doc_id.as_str();
        if e.origin != Origin::Remote || !id.starts_with(&prefix) || got.contains_key(id) {
            continue;
        }
        let arrived = now_ms();
        if let Some(doc) = bench.get(id).await?
            && let Some(sent) = doc.to_json()["sent_ms"].as_u64()
        {
            got.insert(id.to_owned(), arrived.saturating_sub(sent));
        }
    }
    ha.stop().await;
    Ok(summary(got.into_values().collect(), o.count, connect_ms))
}

struct GoogleSetup {
    project: Option<String>,
    client: Option<String>,
    device_client: Option<String>,
    browser: bool,
    login: bool,
}

/// One line from stdin, trimmed; `default` if empty.
fn ask(question: &str, default: Option<&str>) -> CliResult<String> {
    use std::io::Write;
    match default {
        Some(d) => eprint!("{question} [{d}]: "),
        None => eprint!("{question}: "),
    }
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| CliError::io("cannot read stdin", e))?;
    let line = line.trim();
    match (line.is_empty(), default) {
        (true, Some(d)) => Ok(d.to_owned()),
        (true, None) => Err(CliError::usage("no answer given")),
        _ => Ok(line.to_owned()),
    }
}

/// Prints a step with its console page and waits for Enter.
fn step(n: u32, title: &str, url: &str, todo: &[&str], browser: bool) -> CliResult<()> {
    eprintln!("\nStep {n}/6: {title}\n  {url}");
    for t in todo {
        eprintln!("  - {t}");
    }
    if browser {
        let _ = open::that_detached(url);
    }
    ask("Press Enter when done", Some(""))?;
    Ok(())
}

/// Reads, checks and stores a client file of the expected kind.
fn take_client(
    ctx: &Ctx,
    path: Option<String>,
    want: setup::ClientKind,
    project: &str,
) -> CliResult<String> {
    let guess = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join("Downloads"))
        .and_then(|d| setup::newest_client_file(&d))
        .map(|p| p.display().to_string());
    let explicit = path.is_some();
    loop {
        let path = match &path {
            Some(p) => p.clone(),
            None => ask("Path of the downloaded JSON file", guess.as_deref())?,
        };
        let bytes =
            std::fs::read(&path).map_err(|e| CliError::io(&format!("cannot read {path}"), e))?;
        let checked =
            setup::inspect(&bytes).and_then(|f| setup::expect(&f, want, project).map(|()| f));
        match checked {
            Ok(f) => {
                let kind = if want == setup::ClientKind::Device {
                    "device"
                } else {
                    "desktop"
                };
                write_private(&ctx.paths.client_file(kind), &bytes)?;
                eprintln!("  stored the {} client {}", want.label(), f.id);
                return Ok(f.id);
            }
            // A file named on the command line is not asked for again.
            Err(e) if explicit => return Err(CliError::usage(format!("{path}: {e}"))),
            Err(e) => eprintln!("  {path}: {e}"),
        }
    }
}

/// `bothy setup google`.
async fn setup_google(ctx: &Ctx, o: GoogleSetup) -> CliResult<()> {
    use setup::{ClientKind, Page, console_url};
    eprintln!(
        "This sets up a Google Cloud project for Bothy (about ten minutes, free). Every device of a\n\
         database must use clients of the same project, so use this project for all of them."
    );
    let project = match o.project {
        Some(p) => p,
        None => {
            step(
                1,
                "Create a project",
                &console_url(Page::CreateProject, ""),
                &[
                    "name it, for example my-notes-sync, and create it",
                    "copy its project id (shown under the name)",
                ],
                o.browser,
            )?;
            ask("Project id", None)?
        }
    };
    setup::check_project_id(&project).map_err(CliError::usage)?;
    let given_client = o.client.is_some();
    if !given_client {
        step(
            2,
            "Enable the Google Drive API",
            &console_url(Page::DriveApi, &project),
            &["click Enable"],
            o.browser,
        )?;
        step(
            3,
            "Consent screen",
            &console_url(Page::Branding, &project),
            &[
                "Get started: an app name and your email; audience External",
                &format!(
                    "then add your Google account as a test user: {}",
                    console_url(Page::Audience, &project)
                ),
            ],
            o.browser,
        )?;
        step(
            4,
            "Scopes",
            &console_url(Page::DataAccess, &project),
            &["Add or remove scopes: .../auth/drive.appdata and .../auth/drive.file, then Save"],
            o.browser,
        )?;
        step(
            5,
            "Desktop client",
            &console_url(Page::CreateClient, &project),
            &["Application type: Desktop app; Create", "Download JSON"],
            o.browser,
        )?;
    }
    let id = take_client(ctx, o.client, ClientKind::Desktop, &project)?;
    if let Some(dc) = o.device_client {
        take_client(ctx, Some(dc), ClientKind::Device, &project)?;
    } else if !given_client
        && ask("Will a headless board (no browser) join? y/N", Some("n"))?.eq_ignore_ascii_case("y")
    {
        eprintln!(
            "  Create one more client of type \"TVs and Limited Input devices\" at {} and download it.",
            console_url(Page::CreateClient, &project)
        );
        take_client(ctx, None, ClientKind::Device, &project)?;
    }
    if !o.login {
        ctx.out(
            json!({"project": project, "client_id": id, "signed_in": false}),
            || "clients stored; sign in with `bothy auth login`".to_owned(),
        );
        return Ok(());
    }
    eprintln!("\nStep 6/6: sign in and check Drive");
    let client = tokens::client(&ctx.paths, "desktop")?;
    let grant = oauth::loopback_login(&client, &[SCOPE_APPDATA, SCOPE_FILE]).await?;
    tokens::save(&ctx.paths, &grant)?;
    let remote = remote_for(ctx, &RemoteSpec::AppData)?;
    match remote.list("").await {
        Ok(files) => {
            ctx.out(
                json!({"project": project, "client_id": id, "signed_in": true, "drive": "ok", "files": files.len()}),
                || {
                    "Google Drive answers. Next: `bothy init --encrypt --remember` here, and\n\
                     `bothy setup google --project <id> --client <same file>` then `bothy join` on other machines.\n\
                     While the consent screen is in Testing, Google ends sign-ins after 7 days; publish it\n\
                     (Audience → Publish app) to avoid that."
                        .to_owned()
                },
            );
            Ok(())
        }
        Err(e) => match setup::explain_drive_error(e.message(), &project) {
            Some(why) => Err(CliError::usage(why)),
            None => Err(e.into()),
        },
    }
}

async fn open(ctx: &Ctx) -> CliResult<Database> {
    let config = CliConfig::load(&ctx.paths)?;
    let entry = entry(ctx, &config)?;
    open_db(ctx, &entry).await
}

fn read_doc(arg: &str) -> CliResult<J> {
    let text = match arg.strip_prefix('@') {
        Some("-") => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|e| CliError::io("cannot read stdin", e))?;
            s
        }
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| CliError::io(&format!("cannot read {path}"), e))?,
        None => arg.to_owned(),
    };
    let j: J =
        serde_json::from_str(&text).map_err(|e| CliError::usage(format!("not valid JSON: {e}")))?;
    if !j.is_object() {
        return Err(CliError::usage("a document must be a JSON object"));
    }
    Ok(j)
}

fn report_text(r: &SyncReport) -> String {
    let mut parts = vec![format!(
        "pushed {} op(s) in {} segment(s); pulled {} op(s) from {} segment(s); {} document(s) changed",
        r.pushed_ops, r.segments_up, r.pulled_ops, r.segments_down, r.changed_docs
    )];
    if r.snapshots_down + r.snapshots_up + r.deleted_files > 0 {
        parts.push(format!(
            "snapshots: {} downloaded, {} applied, {} uploaded; {} old file(s) deleted",
            r.snapshots_down, r.snapshots_applied, r.snapshots_up, r.deleted_files
        ));
    }
    if let Some(id) = r.forked_to {
        parts.push(format!(
            "note: another install used this device id; this device is now {id}"
        ));
    }
    for (d, seq) in &r.gaps {
        parts.push(format!(
            "warning: segment {seq} of device {d} is missing and no snapshot covers it yet"
        ));
    }
    for s in &r.skipped {
        parts.push(format!("warning: skipped invalid remote file {s}"));
    }
    parts.join("\n")
}

fn report_json(r: &SyncReport) -> J {
    json!({
        "pushed_ops": r.pushed_ops,
        "segments_up": r.segments_up,
        "segments_down": r.segments_down,
        "pulled_ops": r.pulled_ops,
        "changed_docs": r.changed_docs,
        "snapshots_down": r.snapshots_down,
        "snapshots_applied": r.snapshots_applied,
        "snapshots_up": r.snapshots_up,
        "deleted_files": r.deleted_files,
        "gaps": r.gaps.iter().map(|(d, s)| json!({"device": d.to_hex(), "seq": s})).collect::<Vec<_>>(),
        "forked_to": r.forked_to.map(|d| d.to_hex()),
        "skipped": r.skipped,
    })
}

fn meta_json(state: &DocState, conflicts: &[bothy::store::Conflict]) -> J {
    let fields: serde_json::Map<String, J> = state
        .fields
        .iter()
        .map(|(name, f)| {
            let mut m = stamp_json(&f.stamp);
            m["value"] = f.value.as_ref().map_or(J::Null, Value::to_json);
            m["unset"] = J::Bool(f.value.is_none());
            m["visible"] =
                J::Bool(f.value.is_some() && state.deleted_at.is_none_or(|d| f.stamp > d));
            (name.to_string(), m)
        })
        .collect();
    let lost: Vec<J> = conflicts
        .iter()
        .map(|c| {
            json!({
                "field": c.field.to_string(),
                "winner": stamp_json(&c.winner),
                "loser": stamp_json(&c.loser),
                "loser_value": c.loser_value.as_ref().map_or(J::Null, Value::to_json),
            })
        })
        .collect();
    json!({
        "deleted_at": state.deleted_at.as_ref().map(stamp_json),
        "fields": fields,
        "conflicts": lost,
    })
}

async fn find_db(remote: &dyn RemoteStore, app_id: &str) -> CliResult<DbId> {
    let mut found: Vec<(bothy::Hlc, DbId)> = Vec::new();
    for e in remote.list("").await? {
        let Some(k) = LogicalKey::parse(&e.key) else {
            continue;
        };
        if k.kind != KeyKind::Manifest {
            continue;
        }
        if let Some(bytes) = remote.get(&e.key).await?
            && let Ok(m) = Manifest::from_json(&bytes, Some(k.db_id))
            && m.app_id == app_id
        {
            found.push((m.created_hlc, m.db_id));
        }
    }
    found.sort();
    match found.as_slice() {
        [] => Err(CliError::usage(format!(
            "no database of app {app_id:?} on this remote; use `bothy init`"
        ))),
        [(_, only)] => Ok(*only),
        [(_, oldest), ..] => {
            eprintln!(
                "note: {} databases of app {app_id:?} found; joining the oldest ({oldest}). Use --db-id to pick another.",
                found.len()
            );
            Ok(*oldest)
        }
    }
}

fn setup_entry(ctx: &mut Ctx, setup: &Setup) -> CliResult<(CliConfig, DbEntry)> {
    if let Some(name) = &setup.name {
        ctx.db = name.clone();
    }
    check_db_name(&ctx.db)?;
    let config = CliConfig::load(&ctx.paths)?;
    if config.dbs.contains_key(&ctx.db) {
        return Err(CliError::usage(format!(
            "local database {:?} already exists; pick another name with --db",
            ctx.db
        )));
    }
    let entry = DbEntry {
        remote: RemoteSpec::parse(&setup.remote, &ctx.db)?,
        app_id: setup.app_id.clone(),
    };
    Ok((config, entry))
}

pub async fn run(mut ctx: Ctx, command: Command) -> CliResult<()> {
    match command {
        Command::Auth(a) => auth(&ctx, a).await,
        Command::Setup(SetupWhat::Google {
            project,
            client,
            device_client,
            no_browser,
            no_login,
        }) => {
            setup_google(
                &ctx,
                GoogleSetup {
                    project,
                    client,
                    device_client,
                    browser: !no_browser,
                    login: !no_login,
                },
            )
            .await
        }

        Command::Init(Init {
            setup,
            encrypt,
            remember,
        }) => {
            let (mut config, entry) = setup_entry(&mut ctx, &setup)?;
            let password = if encrypt {
                Some(secrets::read(ctx.password_stdin, Ask::Create)?)
            } else {
                None
            };
            let db = open_db_with(&ctx, &entry, |c| match &password {
                Some(p) => c.encrypt(p),
                None => c,
            })
            .await?;
            let db_id = db.init().await?;
            config.dbs.insert(ctx.db.clone(), entry.clone());
            config.save(&ctx.paths)?;
            let kept = if remember {
                Some(secrets::remember(&ctx.paths, db_id, &db.export_key()?)?)
            } else {
                None
            };
            ctx.out(json!({"db": ctx.db, "db_id": db_id.to_hex(), "remote": entry.remote.to_config(), "device_id": db.device_id().to_hex(), "encrypted": encrypt, "key_remembered": kept.is_some()}), || {
                let mut s = format!("created database {db_id} as {:?} on {}\nthis device: {}", ctx.db, entry.remote.to_config(), db.device_id());
                s.push_str(&encryption_note(encrypt, kept.as_deref()));
                s
            });
            Ok(())
        }

        Command::Join(Join {
            setup,
            remember,
            db_id,
        }) => {
            let (mut config, entry) = setup_entry(&mut ctx, &setup)?;
            // Ask for a password only if the database turns out to be encrypted.
            let mut password =
                if secrets::available_without_prompt(ctx.password_stdin, Ask::Current) {
                    Some(secrets::read(ctx.password_stdin, Ask::Current)?)
                } else {
                    None
                };
            let with_password = |p: &Option<secrets::Password>| {
                let p = p.clone();
                move |c: Config| match &p {
                    Some(p) => c.password(p),
                    None => c,
                }
            };
            let mut db = open_db_with(&ctx, &entry, with_password(&password)).await?;
            let db_id = match db_id {
                Some(s) => s.parse::<DbId>()?,
                None => find_db(db.remote().as_ref(), &entry.app_id).await?,
            };
            let report = match db.join(db_id).await {
                Err(e) if e.kind() == ErrorKind::KeyRequired && password.is_none() => {
                    password = Some(secrets::read(ctx.password_stdin, Ask::Current)?);
                    drop(db);
                    db = open_db_with(&ctx, &entry, with_password(&password)).await?;
                    db.join(db_id).await?
                }
                r => r?,
            };
            config.dbs.insert(ctx.db.clone(), entry.clone());
            config.save(&ctx.paths)?;
            let encrypted = db.is_encrypted().await?;
            let kept = if remember && encrypted {
                Some(secrets::remember(&ctx.paths, db_id, &db.export_key()?)?)
            } else {
                None
            };
            ctx.out(json!({"db": ctx.db, "db_id": db_id.to_hex(), "device_id": db.device_id().to_hex(), "encrypted": encrypted, "key_remembered": kept.is_some(), "report": report_json(&report)}), || {
                let mut s = format!("joined database {db_id} as {:?}\nthis device: {}\n{}", ctx.db, db.device_id(), report_text(&report));
                s.push_str(&encryption_note(encrypted, kept.as_deref()));
                s
            });
            Ok(())
        }

        Command::Unlock { remember } => {
            let db = open(&ctx).await?;
            let db_id = encrypted_db_id(&db).await?;
            let password = secrets::read(ctx.password_stdin, Ask::Current)?;
            db.unlock(&password).await?;
            let kept = if remember {
                Some(secrets::remember(&ctx.paths, db_id, &db.export_key()?)?)
            } else {
                None
            };
            ctx.out(
                json!({"unlocked": true, "key_remembered": kept.is_some()}),
                || match &kept {
                    Some(w) => format!("password ok; the data key is kept in {w}"),
                    None => {
                        "password ok (add --remember to keep the data key on this machine)".into()
                    }
                },
            );
            Ok(())
        }

        Command::Lock => {
            let db = open(&ctx).await?;
            let db_id = encrypted_db_id(&db).await?;
            let forgotten = secrets::forget(&ctx.paths, db_id);
            ctx.out(json!({"forgotten": forgotten}), || {
                if forgotten {
                    "forgot the data key; commands that sync will ask for the password".into()
                } else {
                    "no data key was kept on this machine".into()
                }
            });
            Ok(())
        }

        Command::Passwd => {
            let db = open(&ctx).await?;
            encrypted_db_id(&db).await?;
            let old = secrets::read(ctx.password_stdin, Ask::Current)?;
            let new = secrets::read(ctx.password_stdin, Ask::Change)?;
            db.change_password(&old, &new).await?;
            ctx.out(json!({"changed": true}), || {
                "password changed; the old one stops working on every device".into()
            });
            Ok(())
        }

        Command::Put {
            collection,
            id,
            document: arg,
            patch,
            unset,
        } => {
            let doc = read_doc(&arg)?;
            let db = open(&ctx).await?;
            let coll = db.collection(&collection)?;
            let changed = if patch {
                let unset: Vec<&str> = unset.iter().map(String::as_str).collect();
                coll.patch(&id, doc, &unset).await?
            } else {
                coll.put(&id, doc).await?
            };
            ctx.out(json!({"changed": changed}), || {
                if changed {
                    String::new()
                } else {
                    "no change".into()
                }
            });
            Ok(())
        }

        Command::Get {
            collection,
            id,
            with_meta,
        } => {
            let db = open(&ctx).await?;
            let coll = db.collection(&collection)?;
            let doc = coll.get(&id).await?;
            if with_meta {
                let state = coll.get_with_meta(&id).await?;
                let conflicts = coll.conflicts(&id).await?;
                let mut out = meta_json(&state, &conflicts);
                out["doc"] = doc.as_ref().map_or(J::Null, Value::to_json);
                if ctx.json {
                    println!("{out}");
                } else {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&out).expect("JSON values always serialize")
                    );
                }
                return Ok(());
            }
            match doc {
                Some(d) if ctx.json => println!("{}", d.to_json()),
                Some(d) => println!(
                    "{}",
                    serde_json::to_string_pretty(&d.to_json())
                        .expect("JSON values always serialize")
                ),
                None => return Err(CliError::other(format!("{collection}/{id} not found"))),
            }
            Ok(())
        }

        Command::List {
            collection,
            limit,
            after,
        } => {
            let db = open(&ctx).await?;
            let mut opts = ListOptions::default().limit(limit);
            if let Some(a) = &after {
                opts = opts.after(a);
            }
            let rows = db.collection(&collection)?.list(opts).await?;
            if ctx.json {
                let arr: Vec<J> = rows
                    .iter()
                    .map(|(id, d)| json!({"id": id.as_str(), "doc": d.to_json()}))
                    .collect();
                println!("{}", J::Array(arr));
            } else {
                for (id, d) in rows {
                    println!("{id}\t{}", d.to_json());
                }
            }
            Ok(())
        }

        Command::Del { collection, id } => {
            let db = open(&ctx).await?;
            let deleted = db.collection(&collection)?.delete(&id).await?;
            ctx.out(json!({"deleted": deleted}), || {
                if deleted {
                    String::new()
                } else {
                    format!("{collection}/{id} did not exist")
                }
            });
            Ok(())
        }

        Command::Sync { watch: false, .. } => {
            let db = open(&ctx).await?;
            unlocked(&ctx, &db).await?;
            let report = db.sync().await?;
            ctx.out(report_json(&report), || report_text(&report));
            Ok(())
        }

        Command::Sync { watch: true, p2p } => {
            let db = open_p2p(&ctx, &p2p, true).await?;
            unlocked(&ctx, &db).await?;
            let mut events = db.subscribe();
            let handle = db.start_auto_sync(AutoSync::default());
            let mut status = handle.watch();
            eprintln!("syncing in the background; press Ctrl-C to stop");
            let mut peers = db.p2p_status().peers;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            // One listener for the whole loop: a new one per turn misses a signal that
            // arrives between turns.
            let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
            loop {
                tokio::select! {
                    _ = &mut ctrl_c => break,
                    _ = tick.tick(), if p2p.p2p => {
                        let now = db.p2p_status().peers;
                        for p in &now {
                            if !peers.iter().any(|q| q == p) {
                                let state = format!("{:?}", p.state).to_lowercase();
                                ctx.out(json!({"event": "peer", "device_id": p.device_id.to_hex(), "state": state}), || {
                                    format!("peer {} {state}", p.device_id)
                                });
                            }
                        }
                        peers = now;
                    }
                    Ok(e) = events.recv() => {
                        if e.origin == Origin::Remote {
                            ctx.out(json!({"event": "change", "collection": e.collection.as_str(), "id": e.doc_id.as_str()}), || {
                                format!("changed {}/{}", e.collection, e.doc_id)
                            });
                        }
                    }
                    Ok(()) = status.changed() => {
                        match status.borrow().clone() {
                            bothy::SyncStatus::Failed { kind, message } => eprintln!("sync failed ({kind}): {message}; retrying"),
                            bothy::SyncStatus::Ok(r) if r.segments_up + r.segments_down > 0 => {
                                ctx.out(json!({"event": "sync", "report": report_json(&r)}), || report_text(&r));
                            }
                            _ => {}
                        }
                    }
                }
            }
            handle.stop().await;
            Ok(())
        }

        Command::Status => {
            let config = CliConfig::load(&ctx.paths)?;
            let entry = entry(&ctx, &config)?;
            let db = open_db(&ctx, &entry).await?;
            let st = db.status().await?;
            let db_id = db.db_id().await?;
            let (encrypted, locked) = (db.is_encrypted().await?, db.is_locked().await?);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            // A latest HLC ahead of the wall clock means another device's clock runs ahead (or ours behind).
            let ahead_ms = st.last_hlc.physical_ms().saturating_sub(now);
            let devices: Vec<J> = st
                .devices
                .iter()
                .map(|d| {
                    json!({
                        "device_id": d.device_id.to_hex(),
                        "name": d.name,
                        "watermark": d.watermark,
                        "last_seen": d.last_seen.map(|h| fmt_ms(h.physical_ms())),
                    })
                })
                .collect();
            ctx.out(
                json!({
                    "db": ctx.db,
                    "db_id": db_id.map(|d| d.to_hex()),
                    "remote": entry.remote.to_config(),
                    "device_id": db.device_id().to_hex(),
                    "documents": st.documents,
                    "tombstones": st.tombstones,
                    "unsynced_ops": st.unpacked_ops,
                    "pending_uploads": st.pending_uploads,
                    "last_sync": st.last_sync_ms.map(fmt_ms),
                    "clock_ahead_ms": ahead_ms,
                    "encrypted": encrypted,
                    "locked": locked,
                    "devices": devices,
                }),
                || {
                    let mut s = format!(
                        "database {:?}: {} on {}\nthis device: {}\ndocuments: {} ({} deleted kept as tombstones)\nnot yet synced: {} op(s), {} segment(s) waiting for upload\nlast sync: {}",
                        ctx.db,
                        db_id.map_or("(not attached)".to_owned(), |d| d.to_hex()),
                        entry.remote.to_config(),
                        db.device_id(),
                        st.documents,
                        st.tombstones,
                        st.unpacked_ops,
                        st.pending_uploads,
                        st.last_sync_ms.map_or("never".to_owned(), fmt_ms),
                    );
                    s.push_str(match (encrypted, locked) {
                        (false, _) => "\nencryption: off",
                        (true, false) => "\nencryption: on, unlocked (data key kept on this machine)",
                        (true, true) => "\nencryption: on, locked (syncing asks for the password; `bothy unlock --remember` keeps the key)",
                    });
                    if ahead_ms > 1000 {
                        s.push_str(&format!("\nclock: other devices are {} s ahead of this machine", ahead_ms / 1000));
                    }
                    for d in &st.devices {
                        s.push_str(&format!(
                            "\ndevice {}{}: segments 1..{} applied, last change {}",
                            d.device_id,
                            d.name.as_ref().map_or(String::new(), |n| format!(" ({n})")),
                            d.watermark,
                            d.last_seen.map_or("-".to_owned(), |h| fmt_ms(h.physical_ms())),
                        ));
                    }
                    s
                },
            );
            Ok(())
        }

        Command::Devices(Devices::List) => {
            let db = open(&ctx).await?;
            let db_id = db
                .db_id()
                .await?
                .ok_or_else(|| CliError::usage("database is not attached"))?;
            let mut rows = Vec::new();
            let mut ids: Vec<(DeviceId, bool, Option<String>, u64)> =
                vec![(db.device_id(), true, None, 0)];
            for d in db.devices().await? {
                ids.push((d.device_id, false, d.name, d.watermark));
            }
            for (id, me, label, watermark) in ids {
                let info = match db
                    .remote()
                    .get(&LogicalKey::device(db_id, id).to_string())
                    .await?
                {
                    Some(b) => DeviceInfo::from_json(&b, id).ok(),
                    None => None,
                };
                rows.push(json!({
                    "device_id": id.to_hex(),
                    "this_device": me,
                    "label": label,
                    "name": info.as_ref().and_then(|i| i.name.clone()),
                    "platform": info.as_ref().and_then(|i| i.platform.clone()),
                    "watermark": watermark,
                }));
            }
            if ctx.json {
                println!("{}", J::Array(rows));
            } else {
                for r in rows {
                    let name = r["label"].as_str().or(r["name"].as_str()).unwrap_or("-");
                    let plat = r["platform"].as_str().unwrap_or("-");
                    let me = if r["this_device"] == true {
                        " (this device)"
                    } else {
                        ""
                    };
                    println!(
                        "{}  {name}  {plat}{me}",
                        r["device_id"].as_str().unwrap_or_default()
                    );
                }
            }
            Ok(())
        }

        Command::Devices(Devices::Rename { device_id, name }) => {
            let db = open(&ctx).await?;
            db.rename_device(device_id.parse()?, &name).await?;
            Ok(())
        }

        Command::Devices(Devices::Prune { device_id, yes }) => {
            if !yes {
                return Err(CliError::usage(
                    "pruning deletes files another device wrote; only do it for a device that is gone for good, and add --yes",
                ));
            }
            let db = open(&ctx).await?;
            unlocked(&ctx, &db).await?;
            let deleted = db.prune_device(device_id.parse()?).await?;
            ctx.out(json!({"deleted": deleted}), || {
                format!(
                    "deleted {} segment(s) after snapshotting them",
                    deleted.len()
                )
            });
            Ok(())
        }

        Command::Snapshot(Snapshot::Create) => {
            let db = open(&ctx).await?;
            unlocked(&ctx, &db).await?;
            let key = db.snapshot().await?;
            ctx.out(json!({"key": key}), || format!("uploaded {key}"));
            Ok(())
        }

        Command::Compact { dry_run } => {
            let db = open(&ctx).await?;
            unlocked(&ctx, &db).await?;
            let r = db.compact(dry_run).await?;
            let verb = if dry_run { "would delete" } else { "deleted" };
            ctx.out(
                json!({"dry_run": dry_run, "segments": r.segments, "snapshots": r.snapshots}),
                || {
                    format!(
                        "{verb} {} segment(s) and {} snapshot(s)",
                        r.segments.len(),
                        r.snapshots.len()
                    )
                },
            );
            Ok(())
        }

        Command::Conflicts {
            since: _,
            clear: true,
        } => {
            let db = open(&ctx).await?;
            let n = db.clear_conflicts().await?;
            ctx.out(json!({"cleared": n}), || {
                format!("cleared {n} conflict record(s)")
            });
            Ok(())
        }

        Command::Conflicts {
            since,
            clear: false,
        } => {
            let db = open(&ctx).await?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            let since = since.map(|s| parse_since(&s, now)).transpose()?;
            let rows = db.conflicts(since).await?;
            if ctx.json {
                println!("{}", J::Array(rows.iter().map(conflict_json).collect()));
            } else if rows.is_empty() {
                println!("no conflicts");
            } else {
                for c in &rows {
                    let lost = c
                        .loser_value
                        .as_ref()
                        .map_or("(unset)".to_owned(), |v| v.to_json().to_string());
                    println!(
                        "{}\t{}/{}\t{}\tlost {} (device {}) to device {}",
                        fmt_ms(c.at_ms),
                        c.collection,
                        c.doc_id,
                        c.field,
                        lost,
                        &c.loser.device.to_hex()[..8],
                        &c.winner.device.to_hex()[..8],
                    );
                }
            }
            Ok(())
        }

        Command::Inspect(Inspect::Remote) => {
            let db = open(&ctx).await?;
            let db_id = db
                .db_id()
                .await?
                .ok_or_else(|| CliError::usage("database is not attached"))?;
            let files = db.remote().list(&format!("{db_id}/")).await?;
            let mut kinds: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
            let mut rows = Vec::new();
            for f in &files {
                let kind = match LogicalKey::parse(&f.key).map(|k| k.kind) {
                    Some(KeyKind::Manifest) => "manifest",
                    Some(KeyKind::Device(_)) => "device",
                    Some(KeyKind::Segment(..)) => "segment",
                    Some(KeyKind::Snapshot(..)) => "snapshot",
                    Some(KeyKind::KeyFile(..)) => "key",
                    Some(KeyKind::Signal(..)) => "signal",
                    Some(_) | None => "unknown",
                };
                let k = kinds.entry(kind).or_default();
                k.0 += 1;
                k.1 += f.size;
                rows.push(json!({"key": f.key, "size": f.size, "kind": kind}));
            }
            if ctx.json {
                println!("{}", J::Array(rows));
            } else {
                for f in &files {
                    println!("{:>10}  {}", f.size, f.key);
                }
                for (kind, (n, bytes)) in kinds {
                    println!("{n} {kind} file(s), {bytes} bytes");
                }
            }
            Ok(())
        }

        Command::Inspect(Inspect::Scan { text }) => {
            let db = open(&ctx).await?;
            let db_id = db
                .db_id()
                .await?
                .ok_or_else(|| CliError::usage("database is not attached"))?;
            let files = db.remote().list(&format!("{db_id}/")).await?;
            let mut hits = Vec::new();
            for f in &files {
                let Some(bytes) = db.remote().get(&f.key).await? else {
                    continue;
                };
                for t in &text {
                    if !t.is_empty() && bytes.windows(t.len()).any(|w| w == t.as_bytes()) {
                        hits.push(json!({"file": f.key, "text": t}));
                    }
                }
            }
            let clean = hits.is_empty();
            ctx.out(json!({"files": files.len(), "found": hits}), || {
                if clean {
                    format!("scanned {} file(s): none contains the text", files.len())
                } else {
                    hits.iter()
                        .map(|h| {
                            format!(
                                "{} contains {}",
                                h["file"].as_str().unwrap_or(""),
                                h["text"]
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            });
            if clean {
                Ok(())
            } else {
                Err(CliError::other("plain text found in remote files"))
            }
        }

        Command::Bench(Bench::Propagate {
            peer,
            count,
            interval_ms,
            timeout,
            no_inline,
            no_p2p,
            reverse,
            p2p,
        }) => {
            let p2p = P2p {
                p2p: !no_p2p,
                ..p2p
            };
            let opts = BenchOpts {
                count,
                interval: std::time::Duration::from_millis(interval_ms),
                timeout: std::time::Duration::from_secs(timeout),
            };
            let a = open_p2p(&ctx, &p2p, !no_inline).await?;
            unlocked(&ctx, &a).await?;
            let result = match peer {
                Some(name) => {
                    let peer_ctx = Ctx {
                        paths: ctx.paths.clone(),
                        json: ctx.json,
                        db: name,
                        password_stdin: ctx.password_stdin,
                    };
                    let b = open_p2p(&peer_ctx, &p2p, !no_inline).await?;
                    unlocked(&peer_ctx, &b).await?;
                    if a.db_id().await? != b.db_id().await? || a.db_id().await?.is_none() {
                        return Err(CliError::usage(
                            "--peer must be another local database joined to the same remote database",
                        ));
                    }
                    bench_pair(&a, &b, p2p.p2p, &opts).await?
                }
                None if reverse => bench_reverse(&a, p2p.p2p, &opts).await?,
                None => bench_remote(&a, p2p.p2p, &opts).await?,
            };
            ctx.out(result.clone(), || {
                format!(
                    "{} of {} arrived; p50 {} ms, p95 {} ms, max {} ms (p2p {}, inline {})",
                    result["arrived"],
                    count,
                    result["p50_ms"],
                    result["p95_ms"],
                    result["max_ms"],
                    p2p.p2p,
                    !no_inline && p2p.p2p
                )
            });
            Ok(())
        }

        Command::Bench(Bench::Echo { p2p }) => {
            let db = open_p2p(&ctx, &p2p, true).await?;
            unlocked(&ctx, &db).await?;
            let handle = db.start_auto_sync(AutoSync::default());
            let mut events = db.subscribe();
            let bench = db.collection(BENCH)?;
            eprintln!("acknowledging bench writes; press Ctrl-C to stop");
            let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
            loop {
                tokio::select! {
                    _ = &mut ctrl_c => break,
                    Ok(e) = events.recv() => {
                        let id = e.doc_id.as_str().to_owned();
                        if e.origin != Origin::Remote || e.collection.as_str() != BENCH {
                            continue;
                        }
                        if let Some(run) = id.strip_prefix('s') {
                            // `bench propagate --reverse` asks for writes from here.
                            if let Some(doc) = bench.get(&id).await? {
                                let d = doc.to_json();
                                let count = d["count"].as_u64().unwrap_or(0).min(10_000);
                                let pause = d["interval_ms"].as_u64().unwrap_or(1000);
                                let (bench, run) = (bench.clone(), run.to_owned());
                                tokio::spawn(async move {
                                    for i in 0..count {
                                        let _ = bench.put(&format!("q{run}-{i:04}"), json!({"sent_ms": now_ms()})).await;
                                        tokio::time::sleep(std::time::Duration::from_millis(pause)).await;
                                    }
                                });
                            }
                            continue;
                        }
                        if !id.starts_with('p') {
                            continue;
                        }
                        let ack = format!("a{}", &id[1..]);
                        if let Some(doc) = bench.get(&id).await? {
                            let sent = doc.to_json()["sent_ms"].clone();
                            bench.put(&ack, json!({"sent_ms": sent, "recv_ms": now_ms()})).await?;
                        }
                    }
                }
            }
            handle.stop().await;
            Ok(())
        }

        Command::Export { collection, all } => {
            let db = open(&ctx).await?;
            let names = match (collection, all) {
                (Some(c), _) => vec![bothy::CollectionName::new(&c)?],
                (None, true) => db.collections().await?,
                (None, false) => return Err(CliError::usage("give a collection or --all")),
            };
            for name in names {
                let coll = db.collection(name.as_str())?;
                let mut after: Option<String> = None;
                loop {
                    let mut opts = ListOptions::default().limit(1000);
                    if let Some(a) = &after {
                        opts = opts.after(a);
                    }
                    let rows = coll.list(opts).await?;
                    let Some((last, _)) = rows.last() else { break };
                    after = Some(last.to_string());
                    for (id, d) in &rows {
                        println!(
                            "{}",
                            json!({"collection": name.as_str(), "id": id.as_str(), "doc": d.to_json()})
                        );
                    }
                }
            }
            Ok(())
        }
    }
}

async fn auth(ctx: &Ctx, a: Auth) -> CliResult<()> {
    match a {
        Auth::Setup {
            client,
            device_client,
        } => {
            if client.is_none() && device_client.is_none() {
                return Err(CliError::usage(
                    "give --client <file> and/or --device-client <file>",
                ));
            }
            for (kind, file) in [("desktop", client), ("device", device_client)] {
                let Some(file) = file else { continue };
                let bytes = std::fs::read(&file)
                    .map_err(|e| CliError::io(&format!("cannot read {file}"), e))?;
                let c = oauth::Client::from_json(&bytes)?;
                write_private(&ctx.paths.client_file(kind), &bytes)?;
                ctx.out(json!({"client": kind, "client_id": c.id}), || {
                    format!("stored the {kind} client {}", c.id)
                });
            }
            Ok(())
        }
        Auth::Login { device } => {
            let grant = if device {
                let client = tokens::client(&ctx.paths, "device")?;
                oauth::device_login(&client).await?
            } else {
                let client = tokens::client(&ctx.paths, "desktop")?;
                oauth::loopback_login(&client, &[SCOPE_APPDATA, SCOPE_FILE]).await?
            };
            let place = tokens::save(&ctx.paths, &grant)?;
            let mut text = format!("signed in; the sign-in is stored in {place}");
            if !grant.has(SCOPE_APPDATA) {
                text.push_str(
                    "\nnote: this sign-in reaches folder databases only (no drive.appdata)",
                );
            }
            ctx.out(
                json!({"signed_in": true, "client": grant.client, "scopes": grant.scopes}),
                || text,
            );
            Ok(())
        }
        Auth::Logout => {
            if let Some(g) = tokens::load(&ctx.paths)
                && let Err(e) = oauth::revoke(&g.refresh_token).await
            {
                eprintln!("warning: {e}; forgetting the local sign-in anyway");
            }
            tokens::clear(&ctx.paths);
            ctx.out(json!({"signed_in": false}), || "signed out".into());
            Ok(())
        }
        Auth::Status => {
            let grant = tokens::load(&ctx.paths);
            // The stored client files, with the Cloud project each belongs to (never the secret).
            let clients: Vec<(&str, Option<setup::ClientFile>)> = ["desktop", "device"]
                .into_iter()
                .filter_map(|k| {
                    let bytes = std::fs::read(ctx.paths.client_file(k)).ok()?;
                    Some((k, setup::inspect(&bytes).ok()))
                })
                .collect();
            let describe = |kind: &str, file: &Option<setup::ClientFile>| match file {
                Some(f) => format!(
                    "{kind} {} (project {})",
                    f.id,
                    f.project_id.as_deref().unwrap_or("unknown")
                ),
                None => format!("{kind} (unreadable client file)"),
            };
            let listed = if clients.is_empty() {
                "none".to_owned()
            } else {
                clients
                    .iter()
                    .map(|(k, f)| describe(k, f))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            ctx.out(
                json!({
                    "signed_in": grant.is_some(),
                    "client": grant.as_ref().map(|g| g.client.clone()),
                    "scopes": grant.as_ref().map(|g| g.scopes.clone()),
                    "clients_configured": clients.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
                    "clients": clients.iter().map(|(k, f)| json!({
                        "kind": k,
                        "client_id": f.as_ref().map(|f| f.id.clone()),
                        "project": f.as_ref().and_then(|f| f.project_id.clone()),
                    })).collect::<Vec<_>>(),
                    "keyring_account": tokens::account(&ctx.paths),
                }),
                || match &grant {
                    Some(g) => format!(
                        "signed in with the {} client; appdata: {}, folder: {}\nclients: {listed}",
                        g.client,
                        if g.has(SCOPE_APPDATA) { "yes" } else { "no" },
                        if g.has(SCOPE_FILE) { "yes" } else { "no" }
                    ),
                    None => format!("not signed in; clients configured: {listed}"),
                },
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting() {
        assert_eq!(fmt_ms(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(fmt_ms(1_700_000_000_000), "2023-11-14 22:13:20 UTC");
        assert_eq!(fmt_ms(951_782_400_000), "2000-02-29 00:00:00 UTC");
    }

    #[test]
    fn since_accepts_durations_and_utc_dates() {
        let now = 1_700_000_000_000;
        assert_eq!(parse_since("90s", now).unwrap(), now - 90_000);
        assert_eq!(parse_since("2h", now).unwrap(), now - 7_200_000);
        assert_eq!(parse_since("7d", now).unwrap(), now - 7 * 86_400_000);
        assert_eq!(parse_since("2023-11-14 22:13:20", 0).unwrap(), now);
        assert_eq!(parse_since("2023-11-14T22:13:20Z", 0).unwrap(), now);
        assert_eq!(parse_since("2000-02-29", 0).unwrap(), 951_782_400_000);
        assert_eq!(parse_since("1970-01-01T00:01", 0).unwrap(), 60_000);
        for bad in [
            "",
            "h",
            "5y",
            "2023-13-01",
            "2023-11-14T25:00",
            "2023-11-14T10:00:00:00",
            "x-y",
        ] {
            assert!(parse_since(bad, now).is_err(), "{bad:?}");
        }
    }
}
