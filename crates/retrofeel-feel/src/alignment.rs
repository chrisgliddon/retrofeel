use std::collections::HashMap;

use retrofeel_types::TranscriptSegment;
use serde::{Deserialize, Serialize};

use crate::{AlignmentMethod, AlignmentQuality, AlignmentReport};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimedAnchor {
    pub source_seconds: f64,
    pub video_seconds: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub word: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AlignmentOptions {
    pub manual_scale: f64,
    pub manual_offset_seconds: f64,
    pub video_duration_seconds: Option<f64>,
    pub source_audio: String,
    pub tool: String,
    pub model: Option<String>,
}

impl Default for AlignmentOptions {
    fn default() -> Self {
        Self {
            manual_scale: 1.0,
            manual_offset_seconds: 0.0,
            video_duration_seconds: None,
            source_audio: "video.mkv:a:0".into(),
            tool: "retrofeel affine alignment".into(),
            model: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AlignmentOutcome {
    pub report: AlignmentReport,
    pub segments: Vec<TranscriptSegment>,
}

pub fn align_srt(
    source: &[TranscriptSegment],
    anchors: &[TimedAnchor],
    options: &AlignmentOptions,
) -> Result<AlignmentOutcome, String> {
    if source.is_empty() {
        return Err("source transcript has no segments".into());
    }
    if !options.manual_scale.is_finite() || options.manual_scale <= 0.0 {
        return Err("alignment scale must be finite and positive".into());
    }
    if !options.manual_offset_seconds.is_finite() {
        return Err("alignment offset must be finite".into());
    }

    let usable = anchors
        .iter()
        .filter(|anchor| anchor.source_seconds.is_finite() && anchor.video_seconds.is_finite())
        .cloned()
        .collect::<Vec<_>>();
    let (method, scale, offset, inliers) = if usable.len() >= 2 {
        let (scale, offset, inliers) = robust_affine(&usable)?;
        (
            AlignmentMethod::AutomaticWordAnchors,
            scale,
            offset,
            inliers,
        )
    } else {
        (
            AlignmentMethod::ManualAffine,
            options.manual_scale,
            options.manual_offset_seconds,
            usable,
        )
    };

    let mut residuals = inliers
        .iter()
        .map(|anchor| (anchor.video_seconds - (scale * anchor.source_seconds + offset)).abs())
        .collect::<Vec<_>>();
    residuals.sort_by(f64::total_cmp);
    let median = percentile(&residuals, 0.5);
    let p95 = percentile(&residuals, 0.95);
    let maximum = residuals.last().copied();

    let source_start = source
        .first()
        .map(|segment| segment.start_seconds)
        .unwrap_or(0.0);
    let source_end = source
        .last()
        .map(|segment| segment.end_seconds)
        .unwrap_or(source_start);
    let anchor_start = inliers
        .iter()
        .map(|anchor| anchor.source_seconds)
        .min_by(f64::total_cmp);
    let anchor_end = inliers
        .iter()
        .map(|anchor| anchor.source_seconds)
        .max_by(f64::total_cmp);
    let mut extrapolated = Vec::new();
    if let Some(first) = anchor_start.filter(|first| *first > source_start + 1.0) {
        extrapolated.push([source_start, first]);
    }
    if let Some(last) = anchor_end.filter(|last| *last < source_end - 1.0) {
        extrapolated.push([last, source_end]);
    }
    let quality = if method == AlignmentMethod::ManualAffine {
        AlignmentQuality::Estimated
    } else if inliers.len() < 20
        || p95.is_none_or(|value| value > 1.5)
        || (scale - 1.0).abs() > 0.005
    {
        AlignmentQuality::Degraded
    } else if extrapolated.is_empty() {
        AlignmentQuality::Complete
    } else {
        AlignmentQuality::Estimated
    };

    let duration = options.video_duration_seconds.unwrap_or(f64::INFINITY);
    let segments = source
        .iter()
        .map(|segment| {
            let start = (scale * segment.start_seconds + offset)
                .max(0.0)
                .min(duration);
            let end = (scale * segment.end_seconds + offset)
                .max(start)
                .min(duration);
            TranscriptSegment {
                start_seconds: start,
                end_seconds: end,
                text: segment.text.clone(),
            }
        })
        .collect();

    let mut notes = Vec::new();
    if !extrapolated.is_empty() {
        notes.push("Transcript regions outside the matched anchor span are extrapolated.".into());
    }
    if quality == AlignmentQuality::Degraded {
        notes.push("Automatic alignment did not meet RetroFeel's confidence threshold.".into());
    }
    Ok(AlignmentOutcome {
        report: AlignmentReport {
            schema_version: 1,
            method,
            quality,
            scale,
            offset_seconds: offset,
            anchor_count: inliers.len(),
            anchor_start_seconds: anchor_start,
            anchor_end_seconds: anchor_end,
            median_absolute_residual_seconds: median,
            p95_absolute_residual_seconds: p95,
            maximum_absolute_residual_seconds: maximum,
            extrapolated_ranges_seconds: extrapolated,
            source_audio: options.source_audio.clone(),
            tool: options.tool.clone(),
            model: options.model.clone(),
            notes,
        },
        segments,
    })
}

fn robust_affine(anchors: &[TimedAnchor]) -> Result<(f64, f64, Vec<TimedAnchor>), String> {
    let mut offsets = anchors
        .iter()
        .map(|anchor| anchor.video_seconds - anchor.source_seconds)
        .collect::<Vec<_>>();
    offsets.sort_by(f64::total_cmp);
    let initial_offset = percentile(&offsets, 0.5).unwrap_or(0.0);
    let mut inliers = anchors
        .iter()
        .filter(|anchor| {
            ((anchor.video_seconds - anchor.source_seconds) - initial_offset).abs() <= 2.5
        })
        .cloned()
        .collect::<Vec<_>>();
    if inliers.len() < 2 {
        return Err("fewer than two consistent transcript anchors".into());
    }

    for _ in 0..3 {
        let (scale, offset) = ordinary_least_squares(&inliers)?;
        let mut residuals = inliers
            .iter()
            .map(|anchor| (anchor.video_seconds - (scale * anchor.source_seconds + offset)).abs())
            .collect::<Vec<_>>();
        residuals.sort_by(f64::total_cmp);
        let median = percentile(&residuals, 0.5).unwrap_or(0.0);
        let threshold = (median * 4.5).clamp(0.35, 2.0);
        let filtered = inliers
            .iter()
            .filter(|anchor| {
                (anchor.video_seconds - (scale * anchor.source_seconds + offset)).abs() <= threshold
            })
            .cloned()
            .collect::<Vec<_>>();
        if filtered.len() < 2 || filtered.len() == inliers.len() {
            break;
        }
        inliers = filtered;
    }
    let (scale, offset) = ordinary_least_squares(&inliers)?;
    Ok((scale, offset, inliers))
}

fn ordinary_least_squares(anchors: &[TimedAnchor]) -> Result<(f64, f64), String> {
    let count = anchors.len() as f64;
    let mean_x = anchors
        .iter()
        .map(|anchor| anchor.source_seconds)
        .sum::<f64>()
        / count;
    let mean_y = anchors
        .iter()
        .map(|anchor| anchor.video_seconds)
        .sum::<f64>()
        / count;
    let denominator = anchors
        .iter()
        .map(|anchor| (anchor.source_seconds - mean_x).powi(2))
        .sum::<f64>();
    if denominator <= f64::EPSILON {
        return Err("transcript anchors do not span enough source time".into());
    }
    let numerator = anchors
        .iter()
        .map(|anchor| (anchor.source_seconds - mean_x) * (anchor.video_seconds - mean_y))
        .sum::<f64>();
    let scale = numerator / denominator;
    let offset = mean_y - scale * mean_x;
    if !scale.is_finite() || !offset.is_finite() || scale <= 0.0 {
        return Err("transcript anchor fit was not finite and positive".into());
    }
    Ok((scale, offset))
}

fn percentile(sorted: &[f64], quantile: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let index = ((sorted.len() - 1) as f64 * quantile).round() as usize;
    sorted.get(index).copied()
}

#[derive(Debug, Clone)]
struct TimedWord {
    word: String,
    seconds: f64,
}

/// Extract exact, monotonic word anchors from whisper.cpp's JSON output.
/// Common words are deliberately excluded so muffled narration cannot create
/// a plausible-looking but incorrect offset.
pub fn anchors_from_whisper_json(
    source: &[TranscriptSegment],
    whisper_json: &serde_json::Value,
) -> Result<Vec<TimedAnchor>, String> {
    let source_words = transcript_words(source);
    let transcription = whisper_json
        .get("transcription")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "whisper JSON has no transcription array".to_string())?;
    let mut asr_words = Vec::new();
    for segment in transcription {
        let text = segment
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let offsets = segment
            .get("offsets")
            .and_then(serde_json::Value::as_object);
        let start = offsets
            .and_then(|value| value.get("from"))
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0)
            / 1_000.0;
        let end = offsets
            .and_then(|value| value.get("to"))
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(start * 1_000.0)
            / 1_000.0;
        let words = normalized_words(text);
        for (index, word) in words.iter().enumerate() {
            let fraction = (index as f64 + 0.5) / words.len().max(1) as f64;
            asr_words.push(TimedWord {
                word: word.clone(),
                seconds: start + (end - start).max(0.0) * fraction,
            });
        }
    }
    Ok(monotonic_exact_matches(&source_words, &asr_words))
}

fn transcript_words(segments: &[TranscriptSegment]) -> Vec<TimedWord> {
    let mut result = Vec::new();
    for segment in segments {
        let words = normalized_words(&segment.text);
        for (index, word) in words.iter().enumerate() {
            let fraction = (index as f64 + 0.5) / words.len().max(1) as f64;
            result.push(TimedWord {
                word: word.clone(),
                seconds: segment.start_seconds
                    + (segment.end_seconds - segment.start_seconds).max(0.0) * fraction,
            });
        }
    }
    result
}

fn normalized_words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric() && character != '\'')
        .map(|word| word.trim_matches('\'').to_ascii_lowercase())
        .filter(|word| word.len() >= 2)
        .collect()
}

fn monotonic_exact_matches(source: &[TimedWord], target: &[TimedWord]) -> Vec<TimedAnchor> {
    let mut source_counts = HashMap::<&str, usize>::new();
    let mut target_positions = HashMap::<&str, Vec<usize>>::new();
    for word in source {
        *source_counts.entry(&word.word).or_default() += 1;
    }
    for (index, word) in target.iter().enumerate() {
        target_positions.entry(&word.word).or_default().push(index);
    }

    let mut pairs = Vec::<(usize, usize)>::new();
    for (source_index, word) in source.iter().enumerate() {
        let Some(positions) = target_positions.get(word.word.as_str()) else {
            continue;
        };
        let source_count = source_counts.get(word.word.as_str()).copied().unwrap_or(0);
        if word.word.len() < 4 || source_count > 12 || positions.len() > 12 {
            continue;
        }
        for target_index in positions.iter().rev() {
            pairs.push((source_index, *target_index));
        }
    }
    if pairs.is_empty() {
        return Vec::new();
    }

    // Longest increasing subsequence over target indices. Target candidates
    // for each source word are reversed above so one source token cannot be
    // selected more than once.
    let mut tails = Vec::<usize>::new();
    let mut tails_pair = Vec::<usize>::new();
    let mut previous = vec![None; pairs.len()];
    for (pair_index, (_, target_index)) in pairs.iter().enumerate() {
        let position = tails.partition_point(|value| value < target_index);
        if position > 0 {
            previous[pair_index] = Some(tails_pair[position - 1]);
        }
        if position == tails.len() {
            tails.push(*target_index);
            tails_pair.push(pair_index);
        } else {
            tails[position] = *target_index;
            tails_pair[position] = pair_index;
        }
    }
    let Some(mut pair_index) = tails_pair.last().copied() else {
        return Vec::new();
    };
    let mut selected = Vec::new();
    loop {
        selected.push(pairs[pair_index]);
        let Some(prior) = previous[pair_index] else {
            break;
        };
        pair_index = prior;
    }
    selected.reverse();
    selected
        .into_iter()
        .map(|(source_index, target_index)| TimedAnchor {
            source_seconds: source[source_index].seconds,
            video_seconds: target[target_index].seconds,
            word: Some(source[source_index].word.clone()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affine_fit_rejects_outlier_and_reports_extrapolation() {
        let source = vec![TranscriptSegment {
            start_seconds: 0.0,
            end_seconds: 100.0,
            text: "example".into(),
        }];
        let mut anchors = (10..90)
            .step_by(2)
            .map(|value| TimedAnchor {
                source_seconds: value as f64,
                video_seconds: value as f64 * 1.0002 + 18.5,
                word: None,
            })
            .collect::<Vec<_>>();
        anchors.push(TimedAnchor {
            source_seconds: 50.0,
            video_seconds: 500.0,
            word: None,
        });
        let outcome = align_srt(&source, &anchors, &AlignmentOptions::default()).unwrap();
        assert!((outcome.report.offset_seconds - 18.5).abs() < 0.01);
        assert!((outcome.report.scale - 1.0002).abs() < 0.00001);
        assert_eq!(outcome.report.quality, AlignmentQuality::Estimated);
    }
}
