use retrofeel_types::TranscriptSegment;

pub fn parse_srt(text: &str) -> Result<Vec<TranscriptSegment>, String> {
    let normalized = text.replace("\r\n", "\n");
    let mut segments = Vec::new();
    for block in normalized.split("\n\n") {
        let mut lines = block.lines().filter(|line| !line.trim().is_empty());
        let Some(first) = lines.next() else { continue };
        let timing = if first.contains("-->") {
            first
        } else {
            lines
                .next()
                .ok_or_else(|| format!("SRT block has no timestamp: {block}"))?
        };
        let (start, end) = timing
            .split_once("-->")
            .ok_or_else(|| format!("Invalid SRT timestamp: {timing}"))?;
        let text = lines.collect::<Vec<_>>().join(" ").trim().to_string();
        if text.is_empty() {
            continue;
        }
        let start_seconds = parse_timestamp(start.trim())?;
        let end_seconds = parse_timestamp(end.trim())?;
        if !start_seconds.is_finite() || !end_seconds.is_finite() || end_seconds < start_seconds {
            return Err(format!("Invalid SRT interval: {timing}"));
        }
        segments.push(TranscriptSegment {
            start_seconds,
            end_seconds,
            text,
        });
    }
    Ok(segments)
}

fn parse_timestamp(value: &str) -> Result<f64, String> {
    let value = value.replace(',', ".");
    let parts = value.split(':').collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err(format!("Invalid SRT timestamp: {value}"));
    }
    let hours = parts[0]
        .parse::<u64>()
        .map_err(|_| format!("Invalid SRT hours: {value}"))?;
    let minutes = parts[1]
        .parse::<u64>()
        .map_err(|_| format!("Invalid SRT minutes: {value}"))?;
    let seconds = parts[2]
        .parse::<f64>()
        .map_err(|_| format!("Invalid SRT seconds: {value}"))?;
    Ok(hours as f64 * 3_600.0 + minutes as f64 * 60.0 + seconds)
}

pub fn render_srt(segments: &[TranscriptSegment]) -> String {
    let mut output = String::new();
    for (index, segment) in segments.iter().enumerate() {
        output.push_str(&(index + 1).to_string());
        output.push('\n');
        output.push_str(&format!(
            "{} --> {}\n{}\n\n",
            format_timestamp(segment.start_seconds),
            format_timestamp(segment.end_seconds),
            segment.text.trim()
        ));
    }
    output
}

fn format_timestamp(seconds: f64) -> String {
    let millis = (seconds.max(0.0) * 1_000.0).round() as u64;
    let hours = millis / 3_600_000;
    let remainder = millis % 3_600_000;
    let minutes = remainder / 60_000;
    let remainder = remainder % 60_000;
    format!(
        "{hours:02}:{minutes:02}:{:02},{:03}",
        remainder / 1_000,
        remainder % 1_000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_round_trip_preserves_timing_and_text() {
        let source = "1\n00:00:01,250 --> 00:00:02,500\nHello there\n\n";
        let parsed = parse_srt(source).unwrap();
        assert_eq!(parsed[0].start_seconds, 1.25);
        assert_eq!(parse_srt(&render_srt(&parsed)).unwrap(), parsed);
    }
}
