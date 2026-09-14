//! Read the small subset of Steam's clip.pb needed to prove segment identity.
//! Wire fields follow CGameRecordingClipFile / CGameRecordingTimelineMetadata.
//! Unknown fields are skipped; ambiguous, trimmed or unrecognized mappings fail closed.
use anyhow::{bail, Context, Result};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Segment {
    pub recording_id: String,
    pub start_offset_ms: u64,
    pub duration_ms: u64,
    pub recording_zero_timeline_offset_ms: u64,
}
#[derive(Debug, Serialize)]
pub struct Clip {
    pub game_id: String,
    pub timeline_id: String,
    pub first_timeline_start_offset_ms: u64,
    pub segments: Vec<Segment>,
}

#[derive(Clone, Copy)]
enum Field<'a> {
    Number(u64),
    Bytes(&'a [u8]),
}
fn varint(data: &[u8], cursor: &mut usize) -> Result<u64> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let byte = *data.get(*cursor).context("truncated clip protobuf")?;
        *cursor += 1;
        if shift == 63 && byte > 1 {
            bail!("overflow in clip protobuf");
        }
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(value);
        }
    }
    bail!("invalid clip protobuf varint")
}
fn fields(data: &[u8]) -> Result<Vec<(u64, Field<'_>)>> {
    if data.len() > 4 * 1024 * 1024 {
        bail!("clip metadata exceeds 4 MiB");
    }
    let mut cursor = 0;
    let mut result = Vec::new();
    while cursor < data.len() {
        let tag = varint(data, &mut cursor)?;
        if tag >> 3 == 0 {
            bail!("invalid protobuf field zero");
        }
        let value = match tag & 7 {
            0 => Field::Number(varint(data, &mut cursor)?),
            kind @ (1 | 2 | 5) => {
                let len = match kind {
                    1 => 8,
                    5 => 4,
                    _ => usize::try_from(varint(data, &mut cursor)?)?,
                };
                let end = cursor
                    .checked_add(len)
                    .context("clip metadata length overflow")?;
                let bytes = data.get(cursor..end).context("truncated clip field")?;
                cursor = end;
                Field::Bytes(bytes)
            }
            _ => bail!("unsupported clip protobuf wire type"),
        };
        result.push((tag >> 3, value));
    }
    Ok(result)
}
fn unique<'a>(fields: &[(u64, Field<'a>)], key: u64) -> Result<Field<'a>> {
    let matches = fields
        .iter()
        .filter(|(id, _)| *id == key)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        bail!("missing or ambiguous clip field {key}");
    }
    Ok(matches[0].1)
}
fn number(fields: &[(u64, Field<'_>)], key: u64) -> Result<u64> {
    match unique(fields, key)? {
        Field::Number(value) => Ok(value),
        _ => bail!("expected number"),
    }
}
fn string(fields: &[(u64, Field<'_>)], key: u64) -> Result<String> {
    match unique(fields, key)? {
        Field::Bytes(value) => Ok(std::str::from_utf8(value)?.into()),
        _ => bail!("expected string"),
    }
}

pub fn parse(data: &[u8], game_id: &str, timeline_id: &str) -> Result<Clip> {
    let root = fields(data)?;
    if number(&root, 4)?.to_string() != game_id {
        bail!("clip game ID mismatch");
    }
    // Multi-timeline clips need a mapping for each timeline, not one inherited zero.
    let timeline = match unique(&root, 1)? {
        Field::Bytes(bytes) => fields(bytes)?,
        _ => bail!("invalid timeline"),
    };
    if string(&timeline, 1)? != timeline_id || number(&timeline, 2)?.to_string() != game_id {
        bail!("clip timeline identity mismatch");
    }
    let mut segments: Vec<Segment> = Vec::new();
    for (_, field) in timeline.iter().filter(|(key, _)| *key == 5) {
        let record = match field {
            Field::Bytes(bytes) => fields(bytes)?,
            _ => bail!("invalid segment"),
        };
        let segment = Segment {
            recording_id: string(&record, 1)?,
            start_offset_ms: number(&record, 2)?,
            duration_ms: number(&record, 3)?,
            recording_zero_timeline_offset_ms: number(&record, 10)?,
        };
        if segments
            .iter()
            .any(|other| other.recording_id == segment.recording_id)
        {
            bail!("duplicate recording ID in clip");
        }
        segments.push(segment);
    }
    Ok(Clip {
        game_id: game_id.into(),
        timeline_id: timeline_id.into(),
        first_timeline_start_offset_ms: number(&root, 2)?,
        segments,
    })
}

impl Clip {
    pub fn first_frame_boottime_us(
        &self,
        recording_id: &str,
        source_anchor_us: u64,
        archive_first_pts_us: u64,
    ) -> Result<u64> {
        let segment = self
            .segments
            .iter()
            .find(|segment| segment.recording_id == recording_id)
            .context("exact recording absent from clip metadata")?;
        if segment.duration_ms == 0 || archive_first_pts_us != 0 {
            bail!("unrecognized archive PTS origin or empty segment");
        }
        if segment
            .start_offset_ms
            .checked_add(self.first_timeline_start_offset_ms)
            != Some(segment.recording_zero_timeline_offset_ms)
        {
            bail!("trimmed or rebased clip requires an independently validated clock mapping");
        }
        // Metadata proves that this archive begins at the segment's own zero;
        // the live first-video source anchor belongs to that exact segment.
        Ok(source_anchor_us)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn whole_restart_segment_uses_its_source_anchor_not_live_normalized_zero() {
        let clip = Clip {
            game_id: "42".into(),
            timeline_id: "timeline".into(),
            first_timeline_start_offset_ms: 3691,
            segments: vec![Segment {
                recording_id: "fg_42_now_0".into(),
                start_offset_ms: 2499,
                duration_ms: 22877,
                recording_zero_timeline_offset_ms: 6190,
            }],
        };
        assert_eq!(
            clip.first_frame_boottime_us("fg_42_now_0", 54_962_746_483, 0)
                .unwrap(),
            54_962_746_483
        );
        assert!(clip
            .first_frame_boottime_us("fg_42_now", 54_962_746_483, 0)
            .is_err());
        assert!(clip
            .first_frame_boottime_us("fg_42_now_0", 54_962_746_483, 1000)
            .is_err());
        let mut trimmed = clip;
        trimmed.segments[0].start_offset_ms += 1000;
        assert!(trimmed
            .first_frame_boottime_us("fg_42_now_0", 54_962_746_483, 0)
            .is_err());
    }
    #[test]
    fn parses_synthetic_restart_shape_and_aligns_known_button_edge() {
        // Construct synthetic wire metadata explicitly; no captured clip is embedded.
        fn var(mut value: u64) -> Vec<u8> {
            let mut bytes = Vec::new();
            while value >= 128 {
                bytes.push((value as u8 & 127) | 128);
                value >>= 7;
            }
            bytes.push(value as u8);
            bytes
        }
        fn num(key: u64, value: u64) -> Vec<u8> {
            [var(key << 3), var(value)].concat()
        }
        fn bytes(key: u64, value: &[u8]) -> Vec<u8> {
            [var(key << 3 | 2), var(value.len() as u64), value.to_vec()].concat()
        }
        let segment = |id: &str, start, duration, zero| {
            [
                bytes(1, id.as_bytes()),
                num(2, start),
                num(3, duration),
                num(10, zero),
            ]
            .concat()
        };
        let timeline = [
            bytes(1, b"timeline_922337205836316672020260912_141503"),
            num(2, 9223372058363166720),
            bytes(
                5,
                &segment("fg_9223372058363166720_20260912_141507", 208, 2215, 3899),
            ),
            bytes(
                5,
                &segment("fg_9223372058363166720_20260912_141509", 2499, 22877, 6190),
            ),
        ]
        .concat();
        let bytes = [
            bytes(1, &timeline),
            num(2, 3691),
            num(4, 9223372058363166720),
        ]
        .concat();
        let clip = parse(
            &bytes,
            "9223372058363166720",
            "timeline_922337205836316672020260912_141503",
        )
        .unwrap();
        assert_eq!(clip.segments.len(), 2);
        let first = clip
            .first_frame_boottime_us("fg_9223372058363166720_20260912_141509", 54_962_746_483, 0)
            .unwrap();
        // The old arithmetic moved this source edge 2.311681 seconds away.
        let edge = first + 16_000;
        let index = [0, 16_667, 33_333]
            .iter()
            .position(|pts| first + pts >= edge);
        assert_eq!(index, Some(1));
        assert!(parse(&bytes, "42", &clip.timeline_id).is_err());
        assert!(parse(&bytes, &clip.game_id, "another timeline").is_err());
    }

    #[test]
    fn malformed_metadata_is_rejected() {
        for data in [&[0u8][..], &[10, 20, 1], &[128; 11]] {
            assert!(fields(data).is_err());
        }
    }
}
