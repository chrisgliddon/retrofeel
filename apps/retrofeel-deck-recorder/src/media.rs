//! Strict media validation. Packet timestamps alone are not decoded frames.
use crate::config::RecorderConfig;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Read,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Serialize)]
pub struct ValidatedMedia {
    pub schema_version: u32,
    pub validation_method: String,
    pub declared_duration_us: Option<u64>,
    pub frame_pts_us: Vec<u64>,
    pub packet_count: usize,
    pub frame_end_pts_us: u64,
    pub has_audio: bool,
    pub audio_frames_without_pts: usize,
    pub source_sha256: std::collections::BTreeMap<String, String>,
}

// Files are used for output so a full pipe cannot deadlock the timeout loop.
fn probe(
    config: &RecorderConfig,
    source: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<Value> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut child = Command::new(&config.ffprobe)
        .args(["-v", "warning", "-protocol_whitelist", "file,concat"])
        .args(args)
        .args(["-of", "json"])
        .arg(source)
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .spawn()
        .context("failed to start ffprobe")?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("media probe deadline overflow")?;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "media probe timed out after {:.3} seconds",
                timeout.as_secs_f64()
            );
        }
        thread::sleep(Duration::from_millis(20));
    };
    use std::io::{Seek, SeekFrom};
    stderr.seek(SeekFrom::Start(0))?;
    let mut diagnostics = String::new();
    stderr.take(16_384).read_to_string(&mut diagnostics)?;
    if !status.success() || !diagnostics.trim().is_empty() {
        bail!(
            "media decode/probe is incomplete or uncertain: {}",
            diagnostics.trim()
        );
    }
    stdout.seek(SeekFrom::Start(0))?;
    serde_json::from_reader(stdout).context("invalid ffprobe JSON")
}

fn timestamps(value: &Value, key: &str, field: &str) -> Result<Vec<u64>> {
    let rows = value[key]
        .as_array()
        .context("missing ffprobe timestamp array")?;
    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let seconds: f64 = row[field]
            .as_str()
            .context("missing frame/packet PTS")?
            .parse()?;
        if !seconds.is_finite() || seconds < 0.0 || seconds * 1e6 > u64::MAX as f64 {
            bail!("invalid frame/packet PTS");
        }
        result.push((seconds * 1e6).round() as u64);
    }
    if result.is_empty() {
        bail!("no video frames");
    }
    Ok(result)
}

pub(crate) fn validate_timestamps(decoded: &[u64], packets: &[u64]) -> Result<()> {
    if decoded.is_empty() || decoded.windows(2).any(|pair| pair[0] >= pair[1]) {
        bail!("decoded frame PTS must be nonempty and strictly increasing");
    }
    let mut packets = packets.to_vec();
    packets.sort_unstable();
    if decoded != packets {
        bail!(
            "decoded frames and video packets disagree ({} decoded, {} packets)",
            decoded.len(),
            packets.len()
        );
    }
    Ok(())
}

fn hashes(source: &Path) -> Result<std::collections::BTreeMap<String, String>> {
    let mut result = std::collections::BTreeMap::new();
    for entry in fs::read_dir(source.parent().context("source has no parent")?)? {
        let entry = entry?;
        let path = entry.path();
        if path != source && path.extension().is_none_or(|extension| extension != "m4s") {
            continue;
        }
        if !entry.file_type()?.is_file() {
            bail!("media source is not a regular file");
        }
        let mut file = File::open(&path)?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        result.insert(
            entry.file_name().to_string_lossy().into_owned(),
            format!("{:x}", hash.finalize()),
        );
    }
    Ok(result)
}

pub fn validate(config: &RecorderConfig, source: &Path) -> Result<ValidatedMedia> {
    let before = hashes(source)?;
    let dash = if source
        .extension()
        .is_some_and(|extension| extension == "mpd")
    {
        Some(crate::dash::resolve(source)?)
    } else {
        None
    };
    let video = dash
        .as_ref()
        .map_or(source, |streams| streams.video.as_path());
    // clip_timeout_seconds is the source-discovery grace, not a fixed budget
    // for decoding an arbitrarily long recording. Allow one realtime decode
    // plus that grace for a validated, finite DASH duration (at most one day).
    let timeout = Duration::from_secs(config.clip_timeout_seconds.max(1)).saturating_add(
        Duration::from_micros(
            dash.as_ref()
                .map_or(0, |streams| streams.declared_duration_us),
        ),
    );
    let frames = probe(
        config,
        video,
        &[
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=pts_time,duration_time,pkt_duration_time",
        ],
        timeout,
    )?;
    let packets = probe(
        config,
        video,
        &["-select_streams", "v:0", "-show_entries", "packet=pts_time"],
        timeout,
    )?;
    let frame_pts_us = timestamps(&frames, "frames", "pts_time")?;
    let packet_pts_us = timestamps(&packets, "packets", "pts_time")?;
    validate_timestamps(&frame_pts_us, &packet_pts_us)?;
    let last = frames["frames"]
        .as_array()
        .and_then(|rows| rows.last())
        .context("missing final frame")?;
    let duration: f64 = last["duration_time"]
        .as_str()
        .or_else(|| last["pkt_duration_time"].as_str())
        .context("missing final decoded frame duration")?
        .parse()?;
    if !duration.is_finite() || duration <= 0.0 || duration > 60.0 {
        bail!("invalid final frame duration");
    }
    let duration_us = (duration * 1e6).round() as u64;
    let frame_end_pts_us = frame_pts_us
        .last()
        .unwrap()
        .checked_add(duration_us)
        .context("frame end overflow")?;
    if let Some(dash) = &dash {
        // Steam's stop metadata can precede the final presentation timestamp
        // within a delivered frame interval. Treat this as a bounded envelope,
        // not an exact end timestamp. Keep the stricter missing-tail check;
        // packet equality and the exact fragment inventory are still required.
        let last_pts = *frame_pts_us.last().unwrap();
        let final_interval = frame_pts_us
            .iter()
            .rev()
            .nth(1)
            .map_or(duration_us, |previous| last_pts - previous);
        if dash.declared_duration_us.saturating_sub(frame_end_pts_us) > duration_us + 1000
            || last_pts.saturating_sub(dash.declared_duration_us)
                > final_interval.saturating_add(1000)
        {
            bail!("decoded video span disagrees with the declared clip duration (declared={}us, final_pts={last_pts}us, frame_end={frame_end_pts_us}us, final_interval={final_interval}us)", dash.declared_duration_us);
        }
    }

    let mut audio_frames_without_pts = 0;
    let has_audio = if let Some(dash) = &dash {
        if let Some(audio) = &dash.audio {
            let audio_frames = probe(
                config,
                audio,
                &[
                    "-select_streams",
                    "a:0",
                    "-show_entries",
                    "frame=pts_time,nb_samples",
                ],
                timeout,
            )?;
            let rows = audio_frames["frames"]
                .as_array()
                .context("missing decoded audio")?;
            if rows.is_empty()
                || rows
                    .iter()
                    .any(|row| row["nb_samples"].as_u64().unwrap_or(0) == 0)
            {
                bail!("empty/invalid decoded audio frames");
            }
            // AAC can decode multiple frames from one timestamped packet.
            // Decodability establishes presence, not independent audio alignment.
            audio_frames_without_pts = rows
                .iter()
                .filter(|row| row["pts_time"].as_str().is_none())
                .count();
            true
        } else {
            false
        }
    } else {
        let streams = probe(
            config,
            source,
            &["-show_entries", "stream=codec_type"],
            timeout,
        )?;
        streams["streams"]
            .as_array()
            .context("missing stream list")?
            .iter()
            .any(|stream| stream["codec_type"] == "audio")
    };
    if before != hashes(source)? {
        bail!("Steam media changed during validation; retry after finalization");
    }
    Ok(ValidatedMedia {
        schema_version: 1,
        validation_method: if dash.is_some() {
            "declared_steam_dash_fragments"
        } else {
            "container_decode"
        }
        .into(),
        declared_duration_us: dash.as_ref().map(|streams| streams.declared_duration_us),
        frame_pts_us,
        packet_count: packet_pts_us.len(),
        frame_end_pts_us,
        has_audio,
        audio_frames_without_pts,
        source_sha256: before,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn decode_fixture(
        duration: &str,
        points: &[&str],
        frame_duration: &str,
        slow: bool,
    ) -> (tempfile::TempDir, RecorderConfig, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("session.mpd");
        fs::write(&source, format!(r#"<MPD type="static" mediaPresentationDuration="{duration}"><Period><AdaptationSet contentType="video"><Representation id="0"><SegmentTemplate timescale="1000" duration="400000" startNumber="1" initialization="init-stream$RepresentationID$.m4s" media="chunk-stream$RepresentationID$-$Number%05d$.m4s"/></Representation></AdaptationSet></Period></MPD>"#)).unwrap();
        for name in ["init-stream0.m4s", "chunk-stream0-00001.m4s"] {
            fs::write(root.path().join(name), "fixture").unwrap();
        }
        let config = RecorderConfig {
            ffprobe: root.path().join("probe"),
            clip_timeout_seconds: 1,
            ..RecorderConfig::default()
        };
        let delay = if slow {
            "case \"$*\" in *frame=pts_time,duration_time,pkt_duration_time*) sleep 2;; esac\n"
        } else {
            ""
        };
        fs::write(
            &config.ffprobe,
            format!("#!/bin/sh\n{delay}cat \"$0.json\"\n"),
        )
        .unwrap();
        fs::set_permissions(&config.ffprobe, fs::Permissions::from_mode(0o755)).unwrap();
        let rows = points
            .iter()
            .map(|pts| serde_json::json!({"pts_time":pts,"duration_time":frame_duration}))
            .collect::<Vec<_>>();
        fs::write(
            config.ffprobe.with_extension("json"),
            serde_json::to_vec(&serde_json::json!({"frames":rows,"packets":rows})).unwrap(),
        )
        .unwrap();
        (root, config, source)
    }

    #[cfg(unix)]
    #[test]
    fn full_decode_has_a_duration_budget_beyond_clip_discovery_grace() {
        let (_root, config, source) = decode_fixture("PT3S", &["0", "1", "2"], "1", true);
        assert_eq!(validate(&config, &source).unwrap().frame_pts_us.len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn complete_decode_accepts_stop_metadata_within_the_final_frame_interval() {
        let (_root, config, source) = decode_fixture(
            "PT5M43.901S",
            &["0", "343.868012", "343.884776", "343.902352"],
            "0.016666",
            false,
        );
        let media = validate(&config, &source).unwrap();
        assert_eq!(media.frame_end_pts_us, 343_919_018);
        assert_eq!(media.packet_count, 4);
    }

    #[cfg(unix)]
    #[test]
    fn duration_envelope_still_rejects_truncation_and_excess_video() {
        for duration in ["PT5S", "PT0.5S"] {
            let (_root, config, source) = decode_fixture(duration, &["0", "1", "2"], "1", false);
            assert!(validate(&config, &source)
                .unwrap_err()
                .to_string()
                .contains("decoded video span"));
        }
    }

    #[test]
    fn missing_decoded_frame_is_not_rounded_away() {
        let packets = (0..134).map(|n| n * 16667).collect::<Vec<_>>();
        assert!(validate_timestamps(&packets[..133], &packets).is_err());
    }
    #[test]
    fn presentation_order_can_differ_from_packet_order() {
        assert!(validate_timestamps(&[0, 16667, 33333], &[0, 33333, 16667]).is_ok());
        assert!(validate_timestamps(&[0, 0], &[0, 0]).is_err());
    }
    #[test]
    fn missing_or_invalid_pts_is_fatal() {
        for value in [
            serde_json::json!({"frames":[{}]}),
            serde_json::json!({"frames":[{"pts_time":"NaN"}]}),
        ] {
            assert!(timestamps(&value, "frames", "pts_time").is_err());
        }
    }
}
