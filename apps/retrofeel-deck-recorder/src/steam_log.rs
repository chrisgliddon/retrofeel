#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingMode {
    OnDemand,
    Background,
    /// Desktop Steam Game Recording session (no fg_/bg_ recording id).
    DesktopSession,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteamLogEvent {
    RecordingStarted {
        recording_id: String,
        game_id: String,
        mode: RecordingMode,
    },
    VideoAnchor {
        source_pts_us: u64,
        normalized_pts_us: u64,
    },
    RecordingStopped {
        recording_id: String,
    },
    ClipSaved {
        clip_id: String,
        timeline_id: Option<String>,
    },
}

pub fn parse_line(line: &str) -> Option<SteamLogEvent> {
    if let Some(recording_id) = bracket_value(line, "Recording Started [recording=") {
        let (mode, body) = if let Some(body) = recording_id.strip_prefix("fg_") {
            (RecordingMode::OnDemand, body)
        } else if let Some(body) = recording_id.strip_prefix("bg_") {
            (RecordingMode::Background, body)
        } else {
            return None;
        };
        let game_id = body.split('_').next()?.to_string();
        return Some(SteamLogEvent::RecordingStarted {
            recording_id,
            game_id,
            mode,
        });
    }

    if let Some(game_id) = desktop_session_start_game_id(line) {
        return Some(SteamLogEvent::RecordingStarted {
            recording_id: String::new(),
            game_id,
            mode: RecordingMode::DesktopSession,
        });
    }

    if line.contains("First video pts is ") {
        let source = value_between(line, "First video pts is ", "s, first video PTS is ")?;
        let normalized = line
            .split("s, first video PTS is ")
            .nth(1)?
            .split('s')
            .next()?;
        return Some(SteamLogEvent::VideoAnchor {
            source_pts_us: parse_seconds_us(source)?,
            normalized_pts_us: parse_seconds_us(normalized)?,
        });
    }

    if let Some(recording_id) = bracket_value(line, "Recording Stopped [recording=") {
        return Some(SteamLogEvent::RecordingStopped { recording_id });
    }
    if line.contains("Recording Stopped") || line.contains("Stopped game recording session") {
        return Some(SteamLogEvent::RecordingStopped {
            recording_id: String::new(),
        });
    }

    if let Some(rest) = line.split("Attempting to save a clip to ").nth(1) {
        let clip_id = rest.split(" from background").next()?.trim().to_string();
        let timeline_id = rest
            .find("timeline_")
            .map(|start| {
                rest[start..]
                    .split(['[', ' ', ','])
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .filter(|value| !value.is_empty());
        return Some(SteamLogEvent::ClipSaved {
            clip_id,
            timeline_id,
        });
    }

    None
}

fn desktop_session_start_game_id(line: &str) -> Option<String> {
    const MARKER: &str = "Starting new game recording session for ";
    let rest = line.split(MARKER).nth(1)?.trim();
    let game_id = rest
        .split(|character: char| character.is_whitespace() || character == ',' || character == ']')
        .next()?
        .trim();
    (!game_id.is_empty() && game_id.chars().all(|c| c.is_ascii_digit()))
        .then(|| game_id.to_string())
}

fn bracket_value(line: &str, marker: &str) -> Option<String> {
    let value = line.split(marker).nth(1)?.split(']').next()?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn value_between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    text.split(start).nth(1)?.split(end).next()
}

fn parse_seconds_us(value: &str) -> Option<u64> {
    let value = value.trim();
    let (seconds, fraction) = value.split_once('.').unwrap_or((value, ""));
    let seconds = seconds.parse::<u64>().ok()?;
    let mut micros = fraction
        .chars()
        .take(6)
        .collect::<String>()
        .parse::<u64>()
        .unwrap_or(0);
    for _ in fraction.len().min(6)..6 {
        micros *= 10;
    }
    seconds.checked_mul(1_000_000)?.checked_add(micros)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_current_steam_recording_markers() {
        assert_eq!(
            parse_line(
                "[2026-07-30 08:00:35][68000.553699] Game Recording - Recording Started \
                 [recording=fg_9223372041183297536_20260730_150035]"
            ),
            Some(SteamLogEvent::RecordingStarted {
                recording_id: "fg_9223372041183297536_20260730_150035".into(),
                game_id: "9223372041183297536".into(),
                mode: RecordingMode::OnDemand,
            })
        );
        assert_eq!(
            parse_line(
                "[2026-07-30 08:00:35][68000.567709] >>> First video pts is \
                 68017.264810s, first video PTS is 0.010215s"
            ),
            Some(SteamLogEvent::VideoAnchor {
                source_pts_us: 68_017_264_810,
                normalized_pts_us: 10_215,
            })
        );
        assert_eq!(
            parse_line(
                "[2026-07-30 08:09:48][68553.737487] Attempting to save a clip to \
                 clip_9223372041183297536_20260730_150948 from background, \
                 timeline_922337204118329753620260730_150017[17668] to x"
            ),
            Some(SteamLogEvent::ClipSaved {
                clip_id: "clip_9223372041183297536_20260730_150948".into(),
                timeline_id: Some("timeline_922337204118329753620260730_150017".into()),
            })
        );
    }

    #[test]
    fn distinguishes_background_recordings() {
        assert!(matches!(
            parse_line("[x] Game Recording - Recording Started [recording=bg_42_20260730_150035]"),
            Some(SteamLogEvent::RecordingStarted {
                mode: RecordingMode::Background,
                ..
            })
        ));
    }

    #[test]
    fn accepts_stop_markers_without_a_recording_field() {
        assert_eq!(
            parse_line("[x] Game Recording - Recording Stopped"),
            Some(SteamLogEvent::RecordingStopped {
                recording_id: String::new()
            })
        );
    }

    #[test]
    fn parses_desktop_steam_session_lifecycle() {
        assert_eq!(
            parse_line(
                "[2026-07-31 15:09:54][7202.399296] Starting new game recording session for 42"
            ),
            Some(SteamLogEvent::RecordingStarted {
                recording_id: String::new(),
                game_id: "42".into(),
                mode: RecordingMode::DesktopSession,
            })
        );
        assert_eq!(
            parse_line("[2026-07-31 15:11:31][7299.781658] Stopped game recording session"),
            Some(SteamLogEvent::RecordingStopped {
                recording_id: String::new()
            })
        );
    }

    #[test]
    fn ignores_disabled_and_stopped_game_noise() {
        assert_eq!(
            parse_line(
                "Game Recording - would start recording game 42, but recording for this game is disabled"
            ),
            None
        );
        assert_eq!(
            parse_line("Game Recording - game stopped [gameid=42]"),
            None
        );
    }
}
