mod api;
mod auth;
mod crack;
mod crawl;
mod db;
mod export;
mod fetch;
mod geo;
mod refresh;
mod reviews;
mod util;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "labsweep", version, about = "Worldwide Adventure Labs pipeline")]
struct Cli {
    /// SQLite database path.
    #[arg(long, global = true, default_value = "data/labsweep.db")]
    db: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Stage 1: quad-tree discovery of every Adventure Lab.
    Crawl {
        /// Concurrent workers.
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
        /// Max aggregate requests/sec across all workers (0 = unlimited).
        #[arg(long, default_value_t = 10.0)]
        rate: f64,
        /// Number of fibonacci-sphere seed cells.
        #[arg(long, default_value_t = 16)]
        seeds: usize,
        /// Seed cell radius in meters.
        #[arg(long, default_value_t = 12_000_000.0)]
        seed_radius_m: f64,
        /// Below this radius a cell accepts partial results instead of splitting.
        #[arg(long, default_value_t = 1_000.0)]
        min_radius_m: f64,
        /// Stop after N processed cells (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        max_cells: usize,
        /// Wipe the queue + discovered labs and start over.
        #[arg(long)]
        reset: bool,
        /// Re-queue cells marked failed.
        #[arg(long)]
        reset_failed: bool,
    },
    /// Stage 2: fetch the full detail record per discovered GUID.
    Fetch {
        /// Concurrent requests.
        #[arg(long, default_value_t = 16)]
        concurrency: usize,
        /// Max aggregate requests/sec (0 = unlimited).
        #[arg(long, default_value_t = 12.0)]
        rate: f64,
        /// Bearer token — unlocks answer-hash fields in responses.
        #[arg(long, default_value = "")]
        bearer: String,
        /// Fetch at most N GUIDs (0 = all pending).
        #[arg(long, default_value_t = 0)]
        max: usize,
        /// Refresh mode: re-fetch already-fetched details stalest-first.
        #[arg(long, default_value_t = false)]
        restudy: bool,
    },
    /// Stage 3: fetch all reviews for adventures that have them.
    Reviews {
        /// Concurrent requests.
        #[arg(long, default_value_t = 16)]
        concurrency: usize,
        /// Max aggregate requests/sec (0 = unlimited).
        #[arg(long, default_value_t = 12.0)]
        rate: f64,
        /// Manual token override; stored credentials used automatically.
        #[arg(long, default_value = "")]
        bearer: String,
        /// Process at most N adventures (0 = all pending).
        #[arg(long, default_value_t = 0)]
        max: usize,
        /// Refresh mode: re-scrape already-scraped adventures stalest-first.
        #[arg(long, default_value_t = false)]
        revisit: bool,
    },
    /// Refresh pass: re-queue crawl cells, pick up new labs, re-fetch
    /// stalest details, re-scrape stalest reviews. Never deletes —
    /// updates, removals and restores append to version history.
    Refresh {
        /// Max aggregate crawl requests/sec.
        #[arg(long, default_value_t = 10.0)]
        crawl_rate: f64,
        /// Max aggregate fetch requests/sec.
        #[arg(long, default_value_t = 12.0)]
        fetch_rate: f64,
        /// Max aggregate reviews requests/sec.
        #[arg(long, default_value_t = 12.0)]
        reviews_rate: f64,
        /// Re-fetch at most N stalest details (0 = all).
        #[arg(long, default_value_t = 0)]
        fetch_max: usize,
        /// Re-scrape at most N stalest review sets (0 = all).
        #[arg(long, default_value_t = 0)]
        reviews_max: usize,
    },
    /// Stage 4: crack stage answer hashes — md5(publicGuid + normalized).
    /// Candidates are generated locally and checked against every stage
    /// hash at once (shared salt). Idempotent; safe to re-run.
    Crack {
        /// Comma-separated phases: multichoice,numeric,patterns,corpus,
        /// mangle,wordlist ("brute" is controlled by --brute-len instead).
        #[arg(long, default_value = "multichoice,numeric,patterns,corpus,mangle")]
        phases: String,
        /// Wordlist file (one candidate per line) for the wordlist phase.
        #[arg(long)]
        wordlist: Option<PathBuf>,
        /// Brute-force [a-z0-9] up to this length (0 = off). Resumes per
        /// completed length via meta.crack_brute_len.
        #[arg(long, default_value_t = 0)]
        brute_len: usize,
        /// Write unique target hashes as `hash:salt` lines (hashcat -m 20)
        /// and exit.
        #[arg(long)]
        export_hashes: Option<PathBuf>,
        /// Import a hashcat potfile (hash:plain lines) into cracks and exit.
        #[arg(long)]
        import_hashes: Option<PathBuf>,
        /// Override the salt guid (default: meta.auth.public_guid).
        #[arg(long)]
        salt: Option<String>,
    },
    /// Export the static dataset for the web viewer (catalog + detail
    /// shards, gzipped — no server needed).
    Export {
        /// Output directory.
        #[arg(long, default_value = "web/data")]
        out: PathBuf,
    },
    /// Print queue/lab/adventure/stage counts.
    Stats,
    /// Compare local labs count against the API's global totalCount.
    Verify,
    /// Manage stored Geocachngg credentials (auto-refreshing auth).
    Auth {
        #[command(subcommand)]
        cmd: AuthCmd,
    },
}

#[derive(Subcommand)]
enum AuthCmd {
    /// Log in with geocaching.com username + password and store the
    /// token set in the db. Env fallbacks: LABSWEEP_USER / LABSWEEP_PASS.
    Login {
        #[arg(long, env = "LABSWEEP_USER")]
        username: String,
        #[arg(long, env = "LABSWEEP_PASS")]
        password: String,
    },
    /// Show the stored account + token freshness.
    Status,
    /// Forget stored credentials.
    Logout,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let db = db::Db::open(&cli.db)?;

    match cli.cmd {
        Cmd::Crawl {
            concurrency,
            rate,
            seeds,
            seed_radius_m,
            min_radius_m,
            max_cells,
            reset,
            reset_failed,
        } => {
            crawl::run(
                db,
                crawl::Args {
                    concurrency,
                    rate,
                    seeds,
                    seed_radius_m,
                    min_radius_m,
                    max_cells,
                    reset,
                    reset_failed,
                },
            )
            .await
        }
        Cmd::Fetch {
            concurrency,
            rate,
            bearer,
            max,
            restudy,
        } => {
            fetch::run(
                db,
                fetch::Args {
                    concurrency,
                    rate,
                    bearer,
                    max,
                    restudy,
                },
            )
            .await
        }
        Cmd::Reviews {
            concurrency,
            rate,
            bearer,
            max,
            revisit,
        } => {
            reviews::run(
                db,
                reviews::Args {
                    concurrency,
                    rate,
                    bearer,
                    max,
                    revisit,
                },
            )
            .await
        }
        Cmd::Crack {
            phases,
            wordlist,
            brute_len,
            export_hashes,
            import_hashes,
            salt,
        } => {
            crack::run(
                db,
                crack::Args {
                    phases,
                    wordlist,
                    brute_len,
                    export_hashes,
                    import_hashes,
                    salt,
                },
            )
            .await
        }
        Cmd::Export { out } => export::run(db, export::Args { out }).await,
        Cmd::Refresh {
            crawl_rate,
            fetch_rate,
            reviews_rate,
            fetch_max,
            reviews_max,
        } => {
            refresh::run(
                db,
                refresh::Args {
                    crawl_rate,
                    fetch_rate,
                    reviews_rate,
                    fetch_max,
                    reviews_max,
                },
            )
            .await
        }
        Cmd::Stats => {
            println!("{}", db.stats().await?);
            Ok(())
        }
        Cmd::Verify => {
            let client = api::Client::new(5.0, None)?;
            crawl::verify_global(&db, &client).await;
            Ok(())
        }
        Cmd::Auth { cmd } => match cmd {
            AuthCmd::Login { username, password } => {
                let http = auth::bare_client(api::base_headers())?;
                let st = auth::do_login(&http, &username, &password).await?;
                println!("logged in as {}", st.username);
                match &st.public_guid {
                    Some(g) => println!("publicGuid: {g} (crack salt)"),
                    None => println!("warning: /me returned no publicGuid"),
                }
                db.set_meta("auth", serde_json::to_string(&st)?).await?;
                println!("stored in meta — fetch will auto-refresh this token");
                Ok(())
            }
            AuthCmd::Status => {
                match auth::Auth::load(&db, api::base_headers()).await? {
                    Some(a) => {
                        println!("user: {}", a.username().await);
                        println!("publicGuid: {:?}", a.public_guid().await);
                        // token() refreshes if stale — also proves the path works
                        match a.token().await {
                            Ok(_) => println!("token: valid (refreshed if needed)"),
                            Err(e) => println!("token: RENEWAL FAILED — {e}"),
                        }
                    }
                    None => println!("not logged in — run: labsweep auth login --username U --password P"),
                }
                Ok(())
            }
            AuthCmd::Logout => {
                db.del_meta("auth").await?;
                println!("credentials removed");
                Ok(())
            }
        },
    }
}
