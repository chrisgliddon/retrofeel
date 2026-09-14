use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use retrofeel_deck_recorder::{
    export_video, list_sessions, migrate_recording_archives, reconcile_sessions,
    repair_recording_archives, run_doctor, sync_recording_archives, sync_youtube_publishers,
    transcribe_steam_audio, youtube_auth_for, youtube_status_all, DeckArchiveFormat,
    RecorderConfig,
};

#[cfg(target_os = "linux")]
use retrofeel_deck_recorder::WatchOptions;

#[derive(Debug, Parser)]
#[command(
    name = "retrofeel-deck-recorder",
    version,
    about = "Frame-aligned input companion for Steam Game Recording"
)]
struct Cli {
    /// Recorder configuration. Defaults to ~/.config/retrofeel/deck-recorder.ron.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Watch Steam's on-demand recording flow and capture evdev input.
    Watch,
    /// Check Steam logs, media tools, output paths, and virtual input devices.
    Doctor {
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List finalized companion sessions.
    List {
        /// Print machine-readable JSON for pull scripts.
        #[arg(long)]
        json: bool,
    },
    /// Report interrupted partial sessions without deleting them.
    Reconcile {
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Decode the exact declared media fragments and emit a validation receipt.
    ValidateMedia {
        #[arg(long)]
        source: PathBuf,
    },
    /// Reconstruct a stopped session into a new directory; preserve the original.
    DeriveSession {
        #[arg(long)]
        session: String,
        #[arg(long)]
        out: PathBuf,
        /// Transcribe only after exact media/clock validation succeeds.
        #[arg(long)]
        transcribe: bool,
    },
    /// Materialize missing portable videos for configured archive rules.
    SyncArchives {
        /// Print the reconciliation report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Convert archived MKV files to verified MP4 files, one at a time.
    MigrateArchives {
        /// Target archive container.
        #[arg(long, value_enum, default_value_t = ArchiveFormatArgument::Mp4)]
        format: ArchiveFormatArgument,
        /// Delete each exact MKV only after its MP4 and receipt are verified.
        #[arg(long)]
        delete_verified_mkv: bool,
        /// Report eligible archives without writing or deleting files.
        #[arg(long)]
        dry_run: bool,
        /// Print the migration report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Re-encode configured MP4 archives to the current YouTube delivery profile.
    RepairArchives {
        /// Report eligible archives without replacing any files.
        #[arg(long)]
        dry_run: bool,
        /// Print the repair report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Authorize one configured YouTube publisher/channel.
    YoutubeAuth {
        /// Stable publisher ID from configuration.
        #[arg(long)]
        publisher: String,
    },
    /// Check OAuth, expected-channel, and persisted publication state.
    YoutubeStatus {
        /// Report only this publisher; omit to report every publisher.
        #[arg(long)]
        publisher: Option<String>,
        /// Print the status report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Reconcile eligible verified MP4 archives with YouTube.
    SyncYoutube {
        /// Reconcile only this publisher; omit to reconcile every publisher.
        #[arg(long)]
        publisher: Option<String>,
        /// Upload at most one new Private canary while the publisher is disabled.
        #[arg(long, requires = "publisher")]
        private_canary: bool,
        /// Report eligible work without network calls or state changes.
        #[arg(long)]
        dry_run: bool,
        /// Print the synchronization report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remux the referenced Steam recording to a portable Matroska file.
    ExportVideo {
        /// Recording ID shown by `list`.
        #[arg(long)]
        session: String,
        /// Destination file; defaults to video.mkv in the companion directory.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Stream Matroska bytes to stdout, for transfer over SSH.
        #[arg(long, conflicts_with = "output")]
        stdout: bool,
    },
    /// Transcribe a completed Steam recording's mixed audio track to SRT.
    Transcribe {
        /// Exact recording ID shown by `list`; never selects across games.
        #[arg(long)]
        session: String,
        /// Replace an existing completed mixed-audio transcript.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ArchiveFormatArgument {
    Mp4,
}

impl From<ArchiveFormatArgument> for DeckArchiveFormat {
    fn from(value: ArchiveFormatArgument) -> Self {
        match value {
            ArchiveFormatArgument::Mp4 => Self::Mp4,
        }
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("retrofeel_deck_recorder=info"),
    )
    .init();
    let cli = Cli::parse();
    let config = RecorderConfig::load(cli.config.as_deref())?;

    match cli.command {
        Command::Watch => run_watch(config),
        Command::ValidateMedia { source } => {
            let receipt = retrofeel_deck_recorder::media::validate(&config, &source)?;
            println!("{}", serde_json::to_string_pretty(&receipt)?);
            Ok(())
        }
        Command::DeriveSession {
            session,
            out,
            transcribe,
        } => {
            let directory =
                retrofeel_deck_recorder::session::derive_session(&config, &session, &out)?;
            if transcribe {
                let mut derived_config = config.clone();
                derived_config.recordings_dir = out;
                transcribe_steam_audio(&derived_config, &session, false)?;
            }
            println!("{}", directory.display());
            Ok(())
        }
        Command::Doctor { json } => {
            let checks = run_doctor(&config)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&checks)?);
            } else {
                for check in &checks {
                    let marker = if check.ok { "ok" } else { "FAIL" };
                    println!("{marker:>4}  {:<28} {}", check.name, check.detail);
                }
            }
            if checks.iter().any(|check| !check.ok) {
                bail!("one or more required checks failed");
            }
            Ok(())
        }
        Command::List { json } => {
            let sessions = list_sessions(&config)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else if sessions.is_empty() {
                println!("No finalized Steam companion sessions.");
            } else {
                for session in sessions {
                    println!(
                        "{}  game={}  frames={}  {:?}",
                        session.id, session.game_id, session.frame_count, session.status
                    );
                }
            }
            Ok(())
        }
        Command::Reconcile { json } => {
            let findings = reconcile_sessions(&config)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&findings)?);
            } else if findings.is_empty() {
                println!("No interrupted partial sessions.");
            } else {
                for finding in findings {
                    println!("{finding}");
                }
            }
            Ok(())
        }
        Command::SyncArchives { json } => {
            let report = sync_recording_archives(&config)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if report.configured_rules == 0 {
                println!("No recording archive rules are configured.");
            } else if report.already_running {
                println!("Another recording archive reconciliation is already running.");
            } else {
                println!(
                    "Matched {} session(s): archived {}, retained {}, copied {} sidecar(s), unavailable {}, failed {}.",
                    report.matched_sessions,
                    report.archived_videos,
                    report.existing_videos,
                    report.copied_sidecars,
                    report.unavailable_sources,
                    report.failures.len()
                );
            }
            if report.has_failures() {
                bail!("one or more recording archives failed");
            }
            Ok(())
        }
        Command::MigrateArchives {
            format,
            delete_verified_mkv,
            dry_run,
            json,
        } => {
            let report =
                migrate_recording_archives(&config, format.into(), delete_verified_mkv, dry_run)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if report.already_running {
                println!("Another recording archive operation is already running.");
            } else {
                println!(
                    "Found {} MKV(s): planned {}, migrated {}, already verified {}, deleted {}, retained {}, failed {}.",
                    report.discovered_mkvs,
                    report.planned_migrations,
                    report.migrated_mp4s,
                    report.existing_verified_mp4s,
                    report.deleted_mkvs,
                    report.retained_mkvs,
                    report.failures.len()
                );
            }
            if report.has_failures() {
                bail!("one or more archive migrations failed");
            }
            Ok(())
        }
        Command::RepairArchives { dry_run, json } => {
            let report = repair_recording_archives(&config, dry_run)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if report.already_running {
                println!("Another recording archive operation is already running.");
            } else {
                println!(
                    "Found {} MP4(s): planned {}, repaired {}, reverified {}, already current {}, failed {}.",
                    report.discovered_mp4s,
                    report.planned_repairs,
                    report.repaired_mp4s,
                    report.reverified_mp4s,
                    report.already_current_mp4s,
                    report.failures.len()
                );
            }
            if report.has_failures() {
                bail!("one or more archive repairs failed");
            }
            Ok(())
        }
        Command::YoutubeAuth { publisher } => {
            let status = youtube_auth_for(&config, &publisher)?;
            println!("{}", serde_json::to_string_pretty(&status)?);
            Ok(())
        }
        Command::YoutubeStatus { publisher, json } => {
            let statuses = youtube_status_all(&config, publisher.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&statuses)?);
            } else if statuses.is_empty() {
                println!("YouTube publishing is not configured.");
            } else {
                for status in statuses {
                    println!(
                        "publisher={} enabled={} authenticated={} channel_match={} published={} pending={} pruned={}",
                        status.publisher_id.as_deref().unwrap_or("unknown"),
                        status.enabled,
                        status.authenticated,
                        status
                            .channel_matches
                            .map_or_else(|| "unknown".into(), |matches| matches.to_string()),
                        status.published_videos,
                        status.pending_uploads,
                        status.pruned_videos
                    );
                    if let Some(error) = status.error {
                        println!("status: {error}");
                    }
                }
            }
            Ok(())
        }
        Command::SyncYoutube {
            publisher,
            private_canary,
            dry_run,
            json,
        } => {
            let reports =
                sync_youtube_publishers(&config, publisher.as_deref(), dry_run, private_canary)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&reports)?);
            } else if reports.is_empty() {
                println!("YouTube publishing is not configured.");
            } else {
                for report in &reports {
                    println!(
                        "publisher={} scanned={} would_upload={} uploaded={} reconciled={} captions=+{}/~{} pruned={} quota_deferred={} activity_deferred={} failed={}",
                        report.publisher_id.as_deref().unwrap_or("unknown"),
                        report.scanned_sessions,
                        report.would_upload,
                        report.uploaded_videos,
                        report.reconciled_videos,
                        report.captions_uploaded,
                        report.captions_updated,
                        report.pruned_videos,
                        report.quota_deferred,
                        report.deferred_for_activity,
                        report.failures.len()
                    );
                }
            }
            if reports.iter().any(|report| report.has_failures()) {
                bail!("one or more YouTube operations failed or remain unsafe");
            }
            Ok(())
        }
        Command::ExportVideo {
            session,
            output,
            stdout,
        } => export_video(&config, &session, output.as_deref(), stdout),
        Command::Transcribe { session, force } => {
            let path = transcribe_steam_audio(&config, &session, force)?;
            println!("{}", path.display());
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
fn run_watch(config: RecorderConfig) -> Result<()> {
    retrofeel_deck_recorder::watch(config, WatchOptions::default())
}

#[cfg(not(target_os = "linux"))]
fn run_watch(_: RecorderConfig) -> Result<()> {
    bail!("input watching requires Linux evdev; use doctor/list locally")
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command};

    #[test]
    fn youtube_auth_requires_an_explicit_publisher() {
        assert!(Cli::try_parse_from(["retrofeel-deck-recorder", "youtube-auth"]).is_err());

        let cli = Cli::try_parse_from([
            "retrofeel-deck-recorder",
            "youtube-auth",
            "--publisher",
            "variant-hunter",
        ])
        .expect("publisher should satisfy the required argument");
        assert!(matches!(
            cli.command,
            Command::YoutubeAuth { publisher } if publisher == "variant-hunter"
        ));
    }

    #[test]
    fn private_canary_requires_a_single_publisher() {
        assert!(Cli::try_parse_from([
            "retrofeel-deck-recorder",
            "sync-youtube",
            "--private-canary",
        ])
        .is_err());

        let cli = Cli::try_parse_from([
            "retrofeel-deck-recorder",
            "sync-youtube",
            "--publisher",
            "example-meadow-world",
            "--private-canary",
        ])
        .expect("private canary should parse for one explicit publisher");
        assert!(matches!(
            cli.command,
            Command::SyncYoutube {
                publisher: Some(publisher),
                private_canary: true,
                ..
            } if publisher == "example-meadow-world"
        ));
    }
}
