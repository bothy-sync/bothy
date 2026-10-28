//! Command-line arguments.

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "bothy",
    version,
    about = "Serverless, local-first sync over your own Google Drive.",
    long_about = None,
    after_help = "Exit codes: 0 ok, 1 error, 2 usage, 3 sign-in needed, 4 offline, 5 remote inconsistent or format too new."
)]
pub struct Cli {
    /// Local database to use.
    #[arg(long, global = true, env = "BOTHY_DB", default_value = "default")]
    pub db: String,

    /// Machine-readable JSON output.
    #[arg(long, global = true)]
    pub json: bool,

    /// Read passwords from stdin, one per line, instead of prompting.
    #[arg(long, global = true)]
    pub password_stdin: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Google sign-in.
    #[command(subcommand)]
    Auth(Auth),
    /// Guided setup of what Bothy needs, with checks at each step.
    #[command(subcommand)]
    Setup(SetupWhat),
    /// Create a new synced database.
    Init(Init),
    /// Join an existing synced database.
    Join(Join),
    /// Check the password of an encrypted database, and keep its key here with --remember.
    Unlock {
        /// Keep the data key in the system keyring so later commands need no password.
        #[arg(long)]
        remember: bool,
    },
    /// Forget the data key kept by `unlock --remember`.
    Lock,
    /// Change the password of an encrypted database (on every device).
    Passwd,
    /// Write a document (a JSON object, or @file).
    Put {
        collection: String,
        id: String,
        /// JSON object, or @path to read it from a file (@- for stdin).
        #[arg(value_name = "JSON")]
        document: String,
        /// Only set the given fields instead of replacing the document.
        #[arg(long)]
        patch: bool,
        /// Fields to remove (with --patch).
        #[arg(long, value_delimiter = ',', requires = "patch")]
        unset: Vec<String>,
    },
    /// Read a document.
    Get {
        collection: String,
        id: String,
        /// Show field stamps, unset fields, the delete stamp and lost concurrent writes.
        #[arg(long)]
        with_meta: bool,
    },
    /// List documents in id order.
    List {
        collection: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Start after this id.
        #[arg(long)]
        after: Option<String>,
    },
    /// Delete a document.
    Del { collection: String, id: String },
    /// Push local changes and pull remote ones.
    Sync {
        /// Keep syncing in the background until Ctrl-C.
        #[arg(long)]
        watch: bool,
        #[command(flatten)]
        p2p: P2p,
    },
    /// Pending changes, last sync and known devices.
    Status,
    /// Devices of this database.
    #[command(subcommand)]
    Devices(Devices),
    /// Snapshots.
    #[command(subcommand)]
    Snapshot(Snapshot),
    /// Delete this device's files that a snapshot older than the grace period covers.
    Compact {
        /// Only show what would be deleted.
        #[arg(long)]
        dry_run: bool,
    },
    /// Writes from other devices that lost to another write, newest first.
    Conflicts {
        /// Only those recorded since: a duration back from now (30m, 2h, 7d) or a UTC date or
        /// time (2026-10-21, 2026-10-21T14:30).
        #[arg(long)]
        since: Option<String>,
        /// Delete every conflict record.
        #[arg(long, conflicts_with = "since")]
        clear: bool,
    },
    /// Debugging views.
    #[command(subcommand)]
    Inspect(Inspect),
    /// Measurements (for development; output may change).
    #[command(subcommand, hide = true)]
    Bench(Bench),
    /// Print documents as JSON lines, sorted (identical on converged devices).
    Export {
        /// Collection to export.
        collection: Option<String>,
        /// Export every collection.
        #[arg(long, conflicts_with = "collection")]
        all: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SetupWhat {
    /// Create a Google Cloud project and OAuth clients for Bothy, step by step, then sign in
    /// and check that Drive answers.
    Google {
        /// The project id, if the project exists already.
        #[arg(long)]
        project: Option<String>,
        /// The Desktop app client JSON, if it is downloaded already.
        #[arg(long)]
        client: Option<String>,
        /// Also a "TVs and Limited Input devices" client JSON, for headless boards.
        #[arg(long)]
        device_client: Option<String>,
        /// Print the console links instead of opening them.
        #[arg(long)]
        no_browser: bool,
        /// Stop after storing the clients; do not sign in.
        #[arg(long)]
        no_login: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum Auth {
    /// Store the OAuth client JSON files of your own Google Cloud project (without them, Bothy's
    /// official clients are used).
    Setup {
        /// "Desktop app" client, for browser sign-in.
        #[arg(long)]
        client: Option<String>,
        /// "TVs and Limited Input devices" client, for `auth login --device`.
        #[arg(long)]
        device_client: Option<String>,
        /// Forget the stored clients and use Bothy's official ones again.
        #[arg(long, conflicts_with_all = ["client", "device_client"])]
        official: bool,
    },
    /// Sign in with Google.
    Login {
        /// No browser here: show a code to approve on another device (folder databases only).
        #[arg(long)]
        device: bool,
    },
    /// Revoke and forget the sign-in.
    Logout,
    /// Show the sign-in state (never prints tokens).
    Status,
}

/// Direct channels to other online devices while syncing in the background.
#[derive(Args, Debug, Clone)]
pub struct P2p {
    /// With --watch: open direct channels to other online devices, so their writes arrive
    /// within about a second.
    #[arg(long)]
    pub p2p: bool,
    /// STUN servers (host:port, comma-separated) to reach peers outside the local network.
    #[arg(
        long,
        value_delimiter = ',',
        requires = "p2p",
        conflicts_with = "no_stun"
    )]
    pub stun: Vec<String>,
    /// No STUN server: direct channels only on the local network.
    #[arg(long, requires = "p2p")]
    pub no_stun: bool,
    /// Local address to offer to peers instead of the detected one.
    #[arg(long, requires = "p2p", hide = true)]
    pub host: Option<std::net::IpAddr>,
}

#[derive(Subcommand, Debug)]
pub enum Bench {
    /// Time from a write on one device until another device sees it.
    Propagate {
        /// A second local database joined to the same remote database (same machine,
        /// same clock). Without it, `bench echo` must run on the other device.
        #[arg(long)]
        peer: Option<String>,
        /// Number of writes.
        #[arg(long, default_value_t = 50)]
        count: u32,
        /// Pause between writes, in milliseconds.
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
        /// Give up on a write after this many seconds.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// Announce segments over the channel instead of sending them.
        #[arg(long)]
        no_inline: bool,
        /// Measure without direct channels (polling only).
        #[arg(long)]
        no_p2p: bool,
        /// Time writes made by the other device (`bench echo` or the demo app's bench mode)
        /// instead of writes made here.
        #[arg(long, conflicts_with = "peer")]
        reverse: bool,
        #[command(flatten)]
        p2p: P2p,
    },
    /// Answer `bench propagate` from another device: acknowledge each write when it arrives,
    /// and make the writes `bench propagate --reverse` asks for.
    Echo {
        #[command(flatten)]
        p2p: P2p,
    },
}

#[derive(Args, Debug)]
pub struct Setup {
    /// Same as --db.
    #[arg(long)]
    pub name: Option<String>,
    /// appdata (hidden app folder), folder[:<name>] (visible Bothy/<name>), or path:<dir>.
    #[arg(long, default_value = "appdata")]
    pub remote: String,
    /// Application id stored in the manifest.
    #[arg(long, default_value = "dev.bothy.cli")]
    pub app_id: String,
}

#[derive(Args, Debug)]
pub struct Init {
    #[command(flatten)]
    pub setup: Setup,
    /// Encrypt the database with a password (asked twice, or BOTHY_PASSWORD).
    #[arg(long)]
    pub encrypt: bool,
    /// With --encrypt: keep the data key in the system keyring.
    #[arg(long, requires = "encrypt")]
    pub remember: bool,
}

#[derive(Args, Debug)]
pub struct Join {
    #[command(flatten)]
    pub setup: Setup,
    /// If the database is encrypted: keep the data key in the system keyring.
    #[arg(long)]
    pub remember: bool,
    /// Database id to join; by default the oldest database of the app id on the remote.
    #[arg(long)]
    pub db_id: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Devices {
    /// List this device and the devices it has seen.
    List,
    /// Set a local label for a device.
    Rename { device_id: String, name: String },
    /// Delete a lost device's segments after snapshotting them (needs --yes).
    Prune {
        device_id: String,
        /// Confirm: this deletes files another device wrote.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum Snapshot {
    /// Upload a snapshot of the local state now.
    Create,
}

#[derive(Subcommand, Debug)]
pub enum Inspect {
    /// Remote files of this database, with sizes.
    Remote,
    /// Download every remote file of this database and list those that contain the given
    /// text as raw bytes, to check that an encrypted database leaks nothing. Exits 1 if any
    /// file does. Compressed files are not decompressed, and the manifest (app id) and file
    /// names are plain by design.
    Scan {
        /// Text to look for; repeat for several.
        #[arg(required = true)]
        text: Vec<String>,
    },
}
