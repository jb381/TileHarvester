//! GPX track and route parsing for GPS stream inspection.

use std::{fs::File, io::Read, path::Path};

use chrono::{DateTime, SecondsFormat, Utc};
use encoding_rs::{UTF_16BE, UTF_16LE, UTF_8};
use roxmltree::{Document, Node, ParsingOptions};
use serde_json::{json, Value};

use crate::error::{Error, Result};

const MAX_GPX_BYTES: usize = 100 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct GpxSegment {
    pub points: Vec<(f64, f64)>,
    pub times: Vec<f64>,
}

impl GpxSegment {
    pub fn to_stream_json(&self) -> Value {
        let mut stream = json!({"latlng": {"data": self.points}});
        if !self.times.is_empty() && self.times.len() == self.points.len() {
            stream["time"] = json!({"data": self.times});
        }
        stream
    }
}

#[derive(Debug, Clone)]
pub struct GpxTrack {
    pub name: Option<String>,
    pub start_utc: Option<String>,
    pub segments: Vec<GpxSegment>,
}

fn size_error() -> Error {
    Error::Validation("GPX file exceeds the 100 MiB safety limit".into())
}

pub fn parse_gpx_file(path: &Path) -> Result<GpxTrack> {
    if !path.is_file() {
        return Err(Error::Validation(format!(
            "GPX file does not exist: {}",
            path.display()
        )));
    }
    let file = File::open(path)?;
    if file.metadata()?.len() > MAX_GPX_BYTES as u64 {
        return Err(size_error());
    }
    let mut bytes = Vec::new();
    file.take(MAX_GPX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    parse_gpx_bytes(&bytes)
}

fn declared_xml_encoding(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let header_len = bytes.len().min(256);
    let header = &bytes[..header_len];
    let end = header.windows(2).position(|pair| pair == b"?>")?;
    let prefix = std::str::from_utf8(&header[..end + 2]).ok()?;
    let trimmed = prefix.trim_start();
    if !trimmed.starts_with("<?xml") {
        return None;
    }
    let end = trimmed.find("?>")?;
    let decl = &trimmed[..end];
    let idx = decl.find("encoding")?;
    let after = decl[idx + "encoding".len()..].trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    let quote = after.chars().next().filter(|&c| c == '"' || c == '\'')?;
    let rest = &after[quote.len_utf8()..];
    let label_end = rest.find(quote)?;
    let enc = encoding_rs::Encoding::for_label(rest[..label_end].trim().as_bytes())?;
    // A byte stream whose `<?xml` header is already valid ASCII/UTF-8 cannot be UTF-16.
    if enc == UTF_16LE || enc == UTF_16BE {
        return None;
    }
    Some(enc)
}

fn contains_unsafe_declaration(xml: &str) -> bool {
    let bytes = xml.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &bytes[i..];
        if rest.starts_with(b"<!--") {
            if let Some(end) = xml[i + 4..].find("-->") {
                i += 4 + end + 3;
                continue;
            }
            return false;
        }
        if rest.starts_with(b"<![CDATA[") {
            if let Some(end) = xml[i + 9..].find("]]>") {
                i += 9 + end + 3;
                continue;
            }
            return false;
        }
        if rest
            .get(..9)
            .is_some_and(|p| p.eq_ignore_ascii_case(b"<!DOCTYPE"))
            || rest
                .get(..8)
                .is_some_and(|p| p.eq_ignore_ascii_case(b"<!ENTITY"))
        {
            return true;
        }
        i += 1;
    }
    false
}

pub fn parse_gpx_bytes(bytes: &[u8]) -> Result<GpxTrack> {
    if bytes.len() > MAX_GPX_BYTES {
        return Err(size_error());
    }
    let (encoding, offset) = encoding_rs::Encoding::for_bom(bytes).unwrap_or_else(|| {
        // UTF-16 XML can be recognized by its initial code units without a BOM;
        // otherwise honor an explicit XML declaration encoding (e.g. ISO-8859-1).
        let encoding = match bytes {
            [0, b'<' | b' ' | b'\n' | b'\r' | b'\t', ..] => UTF_16BE,
            [b'<' | b' ' | b'\n' | b'\r' | b'\t', 0, ..] => UTF_16LE,
            _ => declared_xml_encoding(bytes).unwrap_or(UTF_8),
        };
        (encoding, 0)
    });
    let xml = encoding
        .decode_without_bom_handling_and_without_replacement(&bytes[offset..])
        .ok_or_else(|| Error::Validation("Invalid GPX XML: invalid XML encoding".into()))?;
    if contains_unsafe_declaration(&xml) {
        return Err(Error::Validation(
            "Unsafe GPX XML: DTD and entity declarations are forbidden".into(),
        ));
    }
    let document = Document::parse_with_options(
        &xml,
        ParsingOptions {
            allow_dtd: false,
            ..ParsingOptions::default()
        },
    )
    .map_err(|error| {
        let prefix = match error {
            roxmltree::Error::DtdDetected
            | roxmltree::Error::UnexpectedEntityCloseTag(_)
            | roxmltree::Error::UnknownEntityReference(..)
            | roxmltree::Error::MalformedEntityReference(_)
            | roxmltree::Error::EntityReferenceLoop(_) => "Unsafe",
            _ => "Invalid",
        };
        Error::Validation(format!("{prefix} GPX XML: {error}"))
    })?;

    let root = document.root_element();
    if root.tag_name().name() != "gpx" {
        return Err(Error::Validation(
            "Invalid GPX XML: expected a gpx root".into(),
        ));
    }
    let mut track = GpxTrack {
        name: None,
        start_utc: None,
        segments: Vec::new(),
    };
    for container in children_named(root, "trk") {
        let mut start = None;
        for segment in children_named(container, "trkseg") {
            append_segment(&mut track, segment, "trkpt", &mut start)?;
        }
        if track.name.is_none() {
            track.name = child_text(container, "name");
        }
    }
    if track.segments.is_empty() {
        for route in children_named(root, "rte") {
            append_segment(&mut track, route, "rtept", &mut None)?;
            if track.name.is_none() {
                track.name = child_text(route, "name");
            }
        }
    }
    Ok(track)
}

fn children_named<'a, 'input>(
    parent: Node<'a, 'input>,
    name: &'static str,
) -> impl Iterator<Item = Node<'a, 'input>> {
    parent.children().filter(move |node| {
        node.is_element()
            && node.tag_name().name() == name
            && node.tag_name().namespace() == parent.tag_name().namespace()
    })
}

fn child_text(parent: Node<'_, '_>, name: &'static str) -> Option<String> {
    let element = children_named(parent, name).next()?;
    let text: String = element
        .children()
        .filter(Node::is_text)
        .filter_map(|node| node.text())
        .collect();
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn coordinate(node: Node<'_, '_>, attribute: &str, limit: f64) -> Result<f64> {
    node.attribute(attribute)
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| (-limit..=limit).contains(value))
        .ok_or_else(|| {
            Error::Validation(format!(
                "Invalid GPX point {attribute} at {}",
                node.document().text_pos_at(node.range().start)
            ))
        })
}

fn append_segment(
    track: &mut GpxTrack,
    node: Node<'_, '_>,
    point_tag: &'static str,
    start: &mut Option<DateTime<Utc>>,
) -> Result<()> {
    let mut points = Vec::new();
    let mut times = Vec::new();
    for point in children_named(node, point_tag) {
        points.push((
            coordinate(point, "lat", 90.0)?,
            coordinate(point, "lon", 180.0)?,
        ));
        if let Some(time) = child_text(point, "time")
            .and_then(|text| DateTime::parse_from_rfc3339(&text).ok())
            .map(|time| time.with_timezone(&Utc))
        {
            let origin = start.get_or_insert(time);
            track
                .start_utc
                .get_or_insert_with(|| origin.to_rfc3339_opts(SecondsFormat::AutoSi, true));
            times.push(time.signed_duration_since(*origin).as_seconds_f64());
        }
    }
    if times.len() != points.len() {
        times.clear();
    }
    track.segments.push(GpxSegment { points, times });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_track_and_segment_boundaries_and_elapsed_time() {
        let track = parse_gpx_bytes(br#"<gpx xmlns="http://www.topografix.com/GPX/1/1" version="1.1">
          <trk><name>Morning &amp; ride</name>
            <trkseg><trkpt lat="12" lon="34"><time>2026-01-01T01:00:00+01:00</time></trkpt></trkseg>
            <trkseg><trkpt lat="13" lon="35"><time>2026-01-01T00:00:10.5Z</time></trkpt></trkseg>
          </trk>
          <trk><trkseg><trkpt lat="14" lon="36"><time>2026-01-02T00:00:00Z</time></trkpt></trkseg></trk>
          <rte><rtept lat="0" lon="0"/></rte>
        </gpx>"#).unwrap();
        assert_eq!(track.name.as_deref(), Some("Morning & ride"));
        assert_eq!(track.start_utc.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(track.segments.len(), 3);
        assert_eq!(track.segments[0].times, [0.0]);
        assert_eq!(
            track.segments[1].to_stream_json(),
            json!({
                "latlng": {"data": [[13.0, 35.0]]}, "time": {"data": [10.5]}
            })
        );
        assert_eq!(track.segments[2].times, [0.0]);
    }

    #[test]
    fn route_fallback_and_untimed_points() {
        let track = parse_gpx_bytes(
            br#"<gpx xmlns="http://www.topografix.com/GPX/1/0">
          <rte><name>Route</name><rtept lat="-90" lon="180"/></rte>
        </gpx>"#,
        )
        .unwrap();
        assert_eq!(track.name.as_deref(), Some("Route"));
        assert_eq!(track.start_utc, None);
        assert_eq!(
            track.segments[0].to_stream_json(),
            json!({"latlng": {"data": [[-90.0, 180.0]]}})
        );
    }

    #[test]
    fn partial_timestamps_omit_segment_time_stream() {
        let track = parse_gpx_bytes(br#"<gpx><trk>
          <trkseg><trkpt lat="0" lon="0"/><trkpt lat="0" lon="0"><time>2026-01-01T00:00:00Z</time></trkpt></trkseg>
          <trkseg><trkpt lat="0" lon="0"><time>2026-01-01T00:00:02Z</time></trkpt></trkseg>
        </trk></gpx>"#).unwrap();
        assert!(track.segments[0].times.is_empty());
        assert_eq!(track.segments[1].times, [2.0]);
    }

    #[test]
    fn rejects_invalid_coordinates() {
        for coordinates in [
            r#"lat="91" lon="0""#,
            r#"lat="NaN" lon="0""#,
            r#"lat="0" lon="inf""#,
            r#"lat="0" lon="181""#,
            r#"lat="0""#,
            r#"lat="north" lon="0""#,
        ] {
            let xml = format!("<gpx><trk><trkseg><trkpt {coordinates}/></trkseg></trk></gpx>");
            assert!(parse_gpx_bytes(xml.as_bytes())
                .unwrap_err()
                .to_string()
                .starts_with("Invalid GPX point"));
        }
    }

    #[test]
    fn rejects_dtd_and_entities_after_decoding() {
        let xml = r#"<!DOCTYPE gpx [<!ENTITY x SYSTEM "file:///example">]><gpx>&x;</gpx>"#;
        let utf16_le: Vec<u8> = [0xff, 0xfe]
            .into_iter()
            .chain(xml.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        let utf16_be: Vec<u8> = xml.encode_utf16().flat_map(u16::to_be_bytes).collect();
        for input in [
            xml.as_bytes(),
            utf16_le.as_slice(),
            utf16_be.as_slice(),
            b"<gpx>&undefined;</gpx>",
        ] {
            assert!(parse_gpx_bytes(input)
                .unwrap_err()
                .to_string()
                .starts_with("Unsafe GPX XML:"));
        }
    }

    #[test]
    fn accepts_comment_cdata_and_legacy_encoding_from_file() {
        let xml = b"<?xml version='1.0' encoding='ISO-8859-1'?><gpx><!-- <!DOCTYPE gpx> --><trk><name>Caf\xe9 <![CDATA[<!ENTITY example>]]></name><trkseg/></trk></gpx>";
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), xml).unwrap();
        let track = parse_gpx_file(file.path()).unwrap();
        assert_eq!(track.name.as_deref(), Some("Café <!ENTITY example>"));
        assert_eq!(track.segments.len(), 1);
    }
}
