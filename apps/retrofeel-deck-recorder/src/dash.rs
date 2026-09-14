//! Resolve only Steam's finite, local, numbered DASH template.
//! Probing concat URLs avoids FFmpeg's DASH demuxer speculatively opening N+1
//! after the declared final fragment. Every declared fragment is still decoded.
use anyhow::{bail, Context, Result};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

pub struct Streams {
    pub video: PathBuf,
    pub audio: Option<PathBuf>,
    pub declared_duration_us: u64,
}

// Steam emits XML durations with minutes/hours for longer recordings, e.g.
// PT10M30.639S. Keep the accepted subset explicit: nonnegative integer hours
// and minutes, optional fractional seconds, in that order, at most one day.
fn duration_us(value: &str) -> Result<u64> {
    let mut remaining = value.strip_prefix("PT").context("expected PT duration")?;
    if remaining.is_empty() {
        bail!("empty duration");
    }
    let mut seconds = 0.0;
    for (unit, multiplier) in [('H', 3600.0), ('M', 60.0), ('S', 1.0)] {
        let Some((number, rest)) = remaining.split_once(unit) else {
            continue;
        };
        let mut parts = number.split('.');
        let integer = parts.next().unwrap_or_default();
        let fraction = parts.next();
        if integer.is_empty()
            || !integer.bytes().all(|b| b.is_ascii_digit())
            || fraction.is_some_and(|digits| {
                unit != 'S' || digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit())
            })
            || parts.next().is_some()
        {
            bail!("invalid {unit} component");
        }
        seconds += number.parse::<f64>()? * multiplier;
        remaining = rest;
    }
    if !remaining.is_empty() || !seconds.is_finite() || seconds <= 0.0 || seconds > 86400.0 {
        bail!("invalid or unsupported duration");
    }
    let micros = (seconds * 1e6).round() as u64;
    if micros == 0 {
        bail!("duration is below microsecond resolution");
    }
    Ok(micros)
}

pub fn resolve(source: &Path) -> Result<Streams> {
    let text = fs::read_to_string(source)?;
    if text.len() > 1024 * 1024 {
        bail!("DASH manifest exceeds 1 MiB");
    }
    let document = roxmltree::Document::parse(&text)?;
    let root = document.root_element();
    if !root.has_tag_name("MPD") || root.attribute("type") != Some("static") {
        bail!("DASH is not finalized/static");
    }
    let duration = root
        .attribute("mediaPresentationDuration")
        .context("missing DASH duration")?;
    let duration_us = duration_us(duration)
        .with_context(|| format!("invalid DASH mediaPresentationDuration {duration:?}"))?;
    let periods = root
        .children()
        .filter(|n| n.has_tag_name("Period"))
        .collect::<Vec<_>>();
    if periods.len() != 1
        || periods[0]
            .attribute("start")
            .is_some_and(|v| v != "PT0.0S" && v != "PT0S")
    {
        bail!("unsupported split-period DASH source");
    }
    let directory = source
        .parent()
        .context("missing source directory")?
        .canonicalize()?;
    let mut video = None;
    let mut audio = None;
    let mut expected_files = BTreeSet::new();
    for adaptation in periods[0]
        .children()
        .filter(|n| n.has_tag_name("AdaptationSet"))
    {
        let slot = match adaptation.attribute("contentType") {
            Some("video") => &mut video,
            Some("audio") => &mut audio,
            _ => bail!("unknown DASH track"),
        };
        if slot.is_some() {
            bail!("ambiguous DASH tracks");
        }
        let representations = adaptation
            .children()
            .filter(|n| n.has_tag_name("Representation"))
            .collect::<Vec<_>>();
        if representations.len() != 1 {
            bail!("ambiguous DASH representations");
        }
        let representation = representations[0];
        let id = representation
            .attribute("id")
            .context("missing representation ID")?;
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            bail!("invalid representation ID");
        }
        let templates = representation
            .children()
            .filter(|n| n.has_tag_name("SegmentTemplate"))
            .collect::<Vec<_>>();
        if templates.len() != 1 {
            bail!("unsupported DASH template placement");
        }
        let template = templates[0];
        if template.children().any(|n| n.is_element())
            || template.attribute("presentationTimeOffset").is_some()
            || template.attribute("initialization") != Some("init-stream$RepresentationID$.m4s")
            || template.attribute("media")
                != Some("chunk-stream$RepresentationID$-$Number%05d$.m4s")
        {
            bail!("unsupported DASH fragment template");
        }
        let timescale: u64 = template
            .attribute("timescale")
            .context("missing timescale")?
            .parse()?;
        let duration: u64 = template
            .attribute("duration")
            .context("missing fragment duration")?
            .parse()?;
        let first: u64 = template
            .attribute("startNumber")
            .context("missing fragment start")?
            .parse()?;
        if timescale == 0 || duration == 0 {
            bail!("invalid DASH timescale/duration");
        }
        let count = (u128::from(duration_us) * u128::from(timescale))
            .div_ceil(u128::from(duration) * 1_000_000);
        if count == 0 || count > 100_000 {
            bail!("invalid DASH fragment count");
        }
        let mut names = vec![format!("init-stream{id}.m4s")];
        for index in 0..count as u64 {
            let number = first
                .checked_add(index)
                .context("fragment number overflow")?;
            names.push(format!("chunk-stream{id}-{number:05}.m4s"));
        }
        let mut paths = Vec::new();
        for name in names {
            let path = directory.join(&name);
            if !path.is_file() || fs::symlink_metadata(&path)?.file_type().is_symlink() {
                bail!("missing/nonregular DASH fragment: {name}");
            }
            let path = path.to_str().context("non-UTF8 media path")?;
            if path.contains(['|', '\n', '\r']) {
                bail!("unsupported media path separator");
            }
            paths.push(path.to_string());
            expected_files.insert(name);
        }
        *slot = Some(PathBuf::from(format!("concat:{}", paths.join("|"))));
    }
    let actual = fs::read_dir(&directory)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|name| name.ends_with(".m4s"))
        .collect::<BTreeSet<_>>();
    if actual != expected_files {
        bail!("DASH fragment inventory disagrees with finalized manifest");
    }
    Ok(Streams {
        video: video.context("missing video representation")?,
        audio,
        declared_duration_us: duration_us,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duration_parser_preserves_fractions_and_rejects_unsupported_syntax() {
        for (value, expected) in [
            ("PT22.877S", 22_877_000),
            ("PT1M", 60_000_000),
            ("PT1H", 3_600_000_000),
            ("PT24H0M0S", 86_400_000_000),
            ("PT0.000001S", 1),
            ("PT1H0.123456S", 3_600_123_456),
        ] {
            assert_eq!(duration_us(value).unwrap(), expected, "{value}");
        }
        for value in [
            "PT",
            "PT0S",
            "PT-1S",
            "PT+1S",
            "PTNaNS",
            "PTinfS",
            "PT1e2S",
            "PT1.5H",
            "PT1.5M",
            "PT1M2H",
            "PT1S2M",
            "PT1M2M",
            "PT1S2S",
            "PT1..2S",
            "PT.2S",
            "PT2.S",
            "PT1M2",
            "PT1Sgarbage",
            "P1DT1S",
            " PT1S",
            "PT25H",
            "PT86400.000001S",
            "PT0.00000001S",
        ] {
            assert!(duration_us(value).is_err(), "{value}");
        }
    }

    #[test]
    fn steam_minute_and_hour_durations_resolve_the_full_fragment_inventory() {
        for (duration, expected_us, count) in [
            ("PT5M43.901S", 343_901_000, 115),
            ("PT7M32.185S", 452_185_000, 151),
            ("PT10M30.639S", 630_639_000, 211),
            ("PT1H2M3.456S", 3_723_456_000, 1242),
        ] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("session.mpd");
            fs::write(&source, format!(r#"<MPD type="static" mediaPresentationDuration="{duration}"><Period start="PT0.0S"><AdaptationSet contentType="video"><Representation id="0"><SegmentTemplate timescale="1000000" duration="3000000" startNumber="1" initialization="init-stream$RepresentationID$.m4s" media="chunk-stream$RepresentationID$-$Number%05d$.m4s"/></Representation></AdaptationSet></Period></MPD>"#)).unwrap();
            fs::write(root.path().join("init-stream0.m4s"), "fixture").unwrap();
            for number in 1..=count {
                fs::write(
                    root.path().join(format!("chunk-stream0-{number:05}.m4s")),
                    "fixture",
                )
                .unwrap();
            }
            let streams = resolve(&source).unwrap_or_else(|error| panic!("{duration}: {error:#}"));
            assert_eq!(streams.declared_duration_us, expected_us);
            assert_eq!(
                streams.video.to_string_lossy().split('|').count(),
                count + 1
            );
            fs::remove_file(root.path().join(format!("chunk-stream0-{count:05}.m4s"))).unwrap();
            assert!(
                resolve(&source).is_err(),
                "must not accept a decoded prefix"
            );
        }
    }

    #[test]
    fn missing_declared_fragment_and_stale_manifest_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("session.mpd");
        fs::write(&source, r#"<MPD type="static" mediaPresentationDuration="PT3.1S"><Period><AdaptationSet contentType="video"><Representation id="0"><SegmentTemplate timescale="1000" duration="3000" startNumber="1" initialization="init-stream$RepresentationID$.m4s" media="chunk-stream$RepresentationID$-$Number%05d$.m4s"/></Representation></AdaptationSet></Period></MPD>"#).unwrap();
        for name in ["init-stream0.m4s", "chunk-stream0-00001.m4s"] {
            fs::write(root.path().join(name), "fixture").unwrap();
        }
        assert!(resolve(&source).is_err());
        fs::write(root.path().join("chunk-stream0-00002.m4s"), "fixture").unwrap();
        let streams = resolve(&source).unwrap();
        assert_eq!(streams.declared_duration_us, 3_100_000);
        assert!(streams
            .video
            .to_string_lossy()
            .ends_with("chunk-stream0-00002.m4s"));
        fs::write(root.path().join("chunk-stream0-00003.m4s"), "fixture").unwrap();
        assert!(resolve(&source).is_err());
    }
}
