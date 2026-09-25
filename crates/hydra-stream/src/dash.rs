// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: MIT OR Apache-2.0

//! MPEG-DASH: MPD parsing and segment-URL generation.
//!
//! Where an HLS media playlist *lists* its segments, an MPD usually
//! **describes** them: a `SegmentTemplate` plus a duration, from which the
//! URLs are generated. So this module's job is arithmetic as much as
//! parsing — work out how many segments a Representation has and spell each
//! one's URL — after which assembly is the same problem HLS already solved
//! and is handed to [`crate::hls::fetch_all`].
//!
//! # Why a hand-written XML scanner
//!
//! An MPD needs four things from XML: element names, attributes, text
//! content (for `BaseURL`), and nesting so `SegmentTemplate` and `BaseURL`
//! can be inherited down the tree. [`scan`] is about a hundred lines and does
//! exactly that, which is a smaller cost than a general XML dependency in a
//! crate whose HTTP client is also hand-written. It is deliberately *not* a
//! conforming XML parser: it reads manifests, not arbitrary documents.
//!
//! # What is deliberately not here
//!
//! No DRM. `ContentProtection` is read only so a protected manifest can be
//! refused by name; no licence request is ever made.

use crate::hls::{Plan, Refusal, Segment, Segments};

/// What a Representation carries. Video and audio are separate in DASH far
/// more often than in HLS, which is why they are kept apart here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
}

/// One Representation, resolved to concrete URLs. A Representation that
/// continues across Periods under the same `id` is one track: the Periods'
/// segments follow each other, and [`Track::init_changes`] marks where a new
/// initialisation segment takes over.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Track {
    pub id: String,
    pub bandwidth: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
    pub init: Option<Segment>,
    pub segments: Vec<Segment>,
    /// `$Number$` for each entry of `segments`. A dynamic manifest republishes
    /// a sliding window, so position means nothing and this is what makes a
    /// segment identifiable across refreshes.
    pub numbers: Vec<u64>,
    /// Each segment's length in seconds, in step with `segments`.
    pub durations: Vec<f64>,
    /// `(index, init)`: the initialisation segment to write before
    /// `segments[index]`, wherever a later Period changed it.
    pub init_changes: Vec<(usize, Segment)>,
    /// Why this Representation resolved to no segments, when the reason is
    /// one to tell the user rather than a quirk of a live window.
    pub unsupported: Option<String>,
}

/// A parsed MPD.
#[derive(Clone, Debug, Default)]
pub struct Manifest {
    /// `mediaPresentationDuration`, seconds.
    pub duration: f64,
    pub live: bool,
    /// `minimumUpdatePeriod`, seconds: how often a dynamic manifest says it
    /// is worth re-reading.
    pub minimum_update_period: Option<f64>,
    pub drm: Option<String>,
    /// Highest bandwidth first.
    pub video: Vec<Track>,
    pub audio: Vec<Track>,
}

/// Ceiling on the segments one Representation may describe. A timeline
/// entry with `r="4000000000"`, or a microsecond duration over a long
/// presentation, would otherwise be expanded into memory as URLs.
pub const MAX_SEGMENTS: u64 = 1_000_000;

// ------------------------------------------------------------ xml scanner

#[derive(Debug, PartialEq)]
enum Node {
    /// `<Tag a="b">`
    Open(String, Vec<(String, String)>),
    /// `<Tag a="b"/>`
    Empty(String, Vec<(String, String)>),
    /// `</Tag>`
    Close(String),
    /// Character data between tags, entities resolved.
    Text(String),
}

/// Drop an XML namespace prefix: `dash:Period` and `Period` are the same
/// element as far as an MPD reader is concerned.
fn local(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_string()
}

fn attributes(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_whitespace() {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != '=' && !bytes[i].is_whitespace() {
            i += 1;
        }
        if start == i {
            break;
        }
        let name: String = bytes[start..i].iter().collect();
        while i < bytes.len() && bytes[i].is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != '=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_whitespace() {
            i += 1;
        }
        let Some(&quote) = bytes.get(i) else { break };
        if quote != '"' && quote != '\'' {
            continue;
        }
        i += 1;
        let vstart = i;
        while i < bytes.len() && bytes[i] != quote {
            i += 1;
        }
        let value: String = bytes[vstart..i].iter().collect();
        i += 1;
        out.push((local(&name), unescape(&value)));
    }
    out
}

/// The five predefined XML entities. An MPD's values are URLs and numbers,
/// so `&amp;` in a query string is the one that actually turns up.
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Tokenize a manifest. Not a conforming XML parser: no DTDs, no entities
/// beyond the predefined five, no CDATA — none of which appear in an MPD.
fn scan(text: &str) -> Vec<Node> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(lt) = rest.find('<') {
        let text_before = &rest[..lt];
        if !text_before.trim().is_empty() {
            out.push(Node::Text(unescape(text_before.trim())));
        }
        rest = &rest[lt + 1..];

        // Declarations, comments and processing instructions carry nothing.
        if rest.starts_with("!--") {
            match rest.find("-->") {
                Some(end) => rest = &rest[end + 3..],
                None => break,
            }
            continue;
        }
        if rest.starts_with('?') || rest.starts_with('!') {
            match rest.find('>') {
                Some(end) => rest = &rest[end + 1..],
                None => break,
            }
            continue;
        }

        let Some(gt) = rest.find('>') else { break };
        let inner = &rest[..gt];
        rest = &rest[gt + 1..];

        if let Some(name) = inner.strip_prefix('/') {
            out.push(Node::Close(local(name.trim())));
            continue;
        }
        let empty = inner.ends_with('/');
        let inner = inner.strip_suffix('/').unwrap_or(inner);
        let split = inner
            .find(|c: char| c.is_whitespace())
            .unwrap_or(inner.len());
        let name = local(inner[..split].trim());
        let attrs = attributes(&inner[split..]);
        out.push(if empty {
            Node::Empty(name, attrs)
        } else {
            Node::Open(name, attrs)
        });
    }
    out
}

// --------------------------------------------------------------- parsing

fn attr<'a>(list: &'a [(String, String)], name: &str) -> Option<&'a str> {
    list.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn num<T: std::str::FromStr>(list: &[(String, String)], name: &str) -> Option<T> {
    attr(list, name).and_then(|v| v.trim().parse().ok())
}

/// `first-last` byte positions, as `mediaRange`, `indexRange` and
/// `Initialization@range` spell them, to `(offset, length)`.
fn byte_range(v: &str) -> Option<(u64, u64)> {
    let (first, last) = v.trim().split_once('-')?;
    let first: u64 = first.trim().parse().ok()?;
    let last: u64 = last.trim().parse().ok()?;
    let len = last.checked_sub(first)?.checked_add(1)?;
    Some((first, len))
}

/// ISO 8601 duration in seconds. Years and months are not meaningful for a
/// presentation and are ignored.
pub fn iso_duration(s: &str) -> Option<f64> {
    let s = s.trim();
    let rest = s.strip_prefix('P')?;
    let (date, time) = match rest.split_once('T') {
        Some((d, t)) => (d, t),
        None => (rest, ""),
    };
    let mut secs = 0.0;
    let mut acc = String::new();
    for c in date.chars() {
        match c {
            'D' => {
                secs += acc.parse::<f64>().unwrap_or(0.0) * 86400.0;
                acc.clear();
            }
            'Y' | 'M' | 'W' => acc.clear(),
            c => acc.push(c),
        }
    }
    acc.clear();
    for c in time.chars() {
        match c {
            'H' => {
                secs += acc.parse::<f64>().unwrap_or(0.0) * 3600.0;
                acc.clear();
            }
            'M' => {
                secs += acc.parse::<f64>().unwrap_or(0.0) * 60.0;
                acc.clear();
            }
            'S' => {
                secs += acc.parse::<f64>().unwrap_or(0.0);
                acc.clear();
            }
            c => acc.push(c),
        }
    }
    (secs > 0.0).then_some(secs)
}

/// Fill `$RepresentationID$`, `$Number$`, `$Time$` and `$Bandwidth$`,
/// honouring the `%0Nd` width DASH allows inside them (`$Number%05d$`).
/// A literal `$$` is an escaped dollar.
fn expand(template: &str, id: &str, number: u64, time: u64, bandwidth: u64) -> String {
    let mut out = String::with_capacity(template.len() + 8);
    let mut rest = template;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        rest = &rest[start + 1..];
        if let Some(stripped) = rest.strip_prefix('$') {
            out.push('$'); // "$$" is a literal dollar
            rest = stripped;
            continue;
        }
        let Some(end) = rest.find('$') else {
            out.push('$');
            out.push_str(rest);
            return out;
        };
        let token = &rest[..end];
        rest = &rest[end + 1..];
        let (name, width) = match token.split_once('%') {
            Some((n, f)) => (
                n,
                f.trim_start_matches('0')
                    .trim_end_matches(['d', 'u', 'x'])
                    .parse::<usize>()
                    .ok()
                    .or_else(|| {
                        // "%05d": the zeros are the width, not padding of it.
                        f.trim_end_matches(['d', 'u', 'x'])
                            .trim_start_matches('0')
                            .is_empty()
                            .then(|| f.trim_end_matches(['d', 'u', 'x']).len())
                    }),
            ),
            None => (token, None),
        };
        let value = match name {
            "RepresentationID" => id.to_string(),
            "Number" => number.to_string(),
            "Time" => time.to_string(),
            "Bandwidth" => bandwidth.to_string(),
            other => {
                // Unknown token: leave it as written rather than silently
                // producing a URL that cannot be right.
                out.push('$');
                out.push_str(other);
                out.push('$');
                continue;
            }
        };
        match width {
            Some(w) if value.len() < w => {
                for _ in 0..w - value.len() {
                    out.push('0');
                }
                out.push_str(&value);
            }
            _ => out.push_str(&value),
        }
    }
    out.push_str(rest);
    out
}

/// Which of the three segment-addressing forms the manifest used last.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Addressing {
    #[default]
    Template,
    List,
    Base,
}

/// One `<S>` of a `SegmentTimeline`, as written: expanded only once the
/// period's length is known, because a negative `r` runs to its end.
#[derive(Clone, Debug, PartialEq)]
struct TimelineEntry {
    t: Option<u64>,
    d: u64,
    r: i64,
}

/// Segment addressing, merged down the tree: a `SegmentTemplate`,
/// `SegmentList` or `SegmentBase` with everything inherited from the
/// levels above it.
#[derive(Clone, Debug, Default)]
struct Template {
    form: Addressing,
    init: Option<String>,
    init_range: Option<(u64, u64)>,
    media: Option<String>,
    timescale: f64,
    duration: Option<f64>,
    start_number: Option<u64>,
    presentation_time_offset: u64,
    timeline: Vec<TimelineEntry>,
    /// `SegmentURL` entries: `(media, mediaRange)`.
    list: Vec<(String, Option<(u64, u64)>)>,
}

impl Template {
    fn merge(&mut self, attrs: &[(String, String)]) {
        if let Some(v) = attr(attrs, "initialization") {
            self.init = Some(v.to_string());
        }
        if let Some(v) = attr(attrs, "media") {
            self.media = Some(v.to_string());
        }
        if let Some(v) = num::<f64>(attrs, "timescale") {
            self.timescale = v;
        }
        if let Some(v) = num::<f64>(attrs, "duration") {
            self.duration = Some(v);
        }
        if let Some(v) = num::<u64>(attrs, "startNumber") {
            self.start_number = Some(v);
        }
        if let Some(v) = num::<u64>(attrs, "presentationTimeOffset") {
            self.presentation_time_offset = v;
        }
    }

    /// Timescale ticks per second.
    fn scale(&self) -> f64 {
        if self.timescale > 0.0 {
            self.timescale
        } else {
            1.0
        }
    }

    fn first_number(&self) -> u64 {
        self.start_number.unwrap_or(1)
    }

    /// `(number, time, seconds)` for every segment this template describes,
    /// or why there are too many to spell out.
    fn ticks(&self, period_seconds: f64) -> Result<Vec<(u64, u64, f64)>, String> {
        let first = self.first_number();
        let scale = self.scale();
        let too_many = || format!("the manifest describes more than {MAX_SEGMENTS} segments");
        if !self.timeline.is_empty() {
            let mut out: Vec<(u64, u64, f64)> = Vec::new();
            let period_end = self
                .presentation_time_offset
                .saturating_add((period_seconds.max(0.0) * scale) as u64);
            let mut start = 0u64;
            for e in &self.timeline {
                if let Some(t) = e.t {
                    start = t;
                }
                // A negative `r` repeats to the end of the period.
                let count = if e.r >= 0 {
                    (e.r as u64).saturating_add(1)
                } else if e.d == 0 || period_end <= start {
                    1
                } else {
                    (period_end - start).div_ceil(e.d).max(1)
                };
                if (out.len() as u64).saturating_add(count) > MAX_SEGMENTS {
                    return Err(too_many());
                }
                for _ in 0..count {
                    out.push((
                        first.saturating_add(out.len() as u64),
                        start,
                        e.d as f64 / scale,
                    ));
                    start = start.saturating_add(e.d);
                }
            }
            return Ok(out);
        }
        let Some(d) = self.duration.filter(|d| *d > 0.0) else {
            return Ok(vec![]);
        };
        let per = d / scale;
        if per <= 0.0 || period_seconds <= 0.0 {
            return Ok(vec![]);
        }
        let count = (period_seconds / per).ceil();
        if count > MAX_SEGMENTS as f64 {
            return Err(too_many());
        }
        let pto = self.presentation_time_offset;
        Ok((0..count as u64)
            .map(|i| {
                (
                    first.saturating_add(i),
                    pto.saturating_add((i as f64 * d) as u64),
                    per,
                )
            })
            .collect())
    }
}

/// One level of the element tree: what a child inherits from it.
#[derive(Clone, Debug, Default)]
struct Scope {
    name: String,
    base: Option<String>,
    template: Option<Template>,
    content_type: Option<String>,
    /// Set on a `<Representation>` scope that is still open. The track is
    /// built when the element CLOSES, because `SegmentTemplate` is frequently
    /// a child of the Representation rather than a sibling on the
    /// AdaptationSet — building at the opening tag would produce a track with
    /// no segments at all.
    pending: Option<Pending>,
}

/// A Representation seen but not yet finished.
#[derive(Clone, Debug)]
struct Pending {
    attrs: Vec<(String, String)>,
    kind: TrackKind,
    base_url: String,
    /// The Representation carried a `<BaseURL>` of its own.
    own_base: bool,
    period: usize,
}

/// `<Period start= duration=>`, as written.
#[derive(Clone, Copy, Debug, Default)]
struct Period {
    start: Option<f64>,
    duration: Option<f64>,
}

/// Each period's length in seconds: its own `@duration`, else up to the next
/// period's `@start`, else to the end of the presentation.
fn period_lengths(periods: &[Period], total: f64) -> Vec<f64> {
    let mut out = Vec::with_capacity(periods.len());
    let mut at = 0.0f64;
    for (i, p) in periods.iter().enumerate() {
        let start = p.start.unwrap_or(at);
        let end = match p.duration {
            Some(d) => start + d,
            None => periods
                .get(i + 1)
                .and_then(|n| n.start)
                .unwrap_or(total.max(start)),
        };
        let secs = (end - start).max(0.0);
        out.push(secs);
        at = start + secs;
    }
    out
}

/// Resolve one Representation into a track.
fn make_track(p: &Pending, template: Option<&Template>, seconds: f64) -> Track {
    let attrs = &p.attrs;
    let id = attr(attrs, "id").unwrap_or("").to_string();
    let bandwidth = num::<u64>(attrs, "bandwidth");
    let mut track = Track {
        id: id.clone(),
        bandwidth,
        width: num(attrs, "width"),
        height: num(attrs, "height"),
        codecs: attr(attrs, "codecs").map(str::to_string),
        ..Track::default()
    };
    let bw = bandwidth.unwrap_or(0);
    let join = |u: &str| crate::url::join(&p.base_url, u);
    let Some(t) = template else {
        if p.own_base {
            // No addressing at all: the Representation is a single file at
            // its BaseURL (ISO 23009-1 §5.3.9.1), which carries its own
            // initialisation.
            track.segments.push(Segment::new(p.base_url.clone()));
            track.numbers.push(1);
            track.durations.push(seconds);
        } else {
            track.unsupported = Some(format!(
                "representation {id} has no segment addressing (no SegmentTemplate, \
                 SegmentList or SegmentBase)"
            ));
        }
        return track;
    };
    let first = t.first_number();
    let init_of = |url: Option<String>| -> Option<Segment> {
        let url = url.or_else(|| t.init_range.map(|_| p.base_url.clone()))?;
        Some(Segment {
            url,
            range: t.init_range,
            key: None,
        })
    };
    match t.form {
        Addressing::Base => {
            // The whole file is the segment and begins with its own `moov`;
            // fetching the `Initialization@range` separately as well would
            // write those bytes twice.
            track.segments.push(Segment::new(p.base_url.clone()));
            track.numbers.push(first);
            track.durations.push(seconds);
        }
        Addressing::List => {
            track.init = init_of(t.init.as_ref().and_then(|i| join(i)));
            let n = t.list.len() as u64;
            if n > MAX_SEGMENTS {
                track.unsupported = Some(format!(
                    "the manifest lists more than {MAX_SEGMENTS} segments"
                ));
                return track;
            }
            let per = t
                .duration
                .filter(|d| *d > 0.0)
                .map(|d| d / t.scale())
                .unwrap_or(if n > 0 { seconds / n as f64 } else { 0.0 });
            for (i, (media, range)) in t.list.iter().enumerate() {
                if let Some(url) = join(media) {
                    track.segments.push(Segment {
                        url,
                        range: *range,
                        key: None,
                    });
                    track.numbers.push(first.saturating_add(i as u64));
                    track.durations.push(per);
                }
            }
            if track.segments.is_empty() {
                track.unsupported = Some("the SegmentList names no SegmentURL".into());
            }
        }
        Addressing::Template => {
            track.init = init_of(
                t.init
                    .as_ref()
                    .map(|i| expand(i, &id, first, 0, bw))
                    .and_then(|u| join(&u)),
            );
            let Some(media) = &t.media else {
                track.unsupported = Some(format!(
                    "representation {id} has a SegmentTemplate without a media attribute"
                ));
                return track;
            };
            let ticks = match t.ticks(seconds) {
                Ok(ticks) => ticks,
                Err(why) => {
                    track.unsupported = Some(why);
                    return track;
                }
            };
            if ticks.is_empty() {
                track.unsupported = Some(format!(
                    "representation {id} has a SegmentTemplate with neither a duration nor a \
                     SegmentTimeline"
                ));
            }
            for (number, time, secs) in ticks {
                if let Some(url) = join(&expand(media, &id, number, time, bw)) {
                    track.segments.push(Segment::new(url));
                    track.numbers.push(number);
                    track.durations.push(secs);
                }
            }
        }
    }
    track
}

fn drm_name(scheme: &str) -> Option<&'static str> {
    let s = scheme.to_ascii_lowercase();
    if s.contains("edef8ba9-79d6-4ace-a3c8-27dcd51d21ed") || s.contains("widevine") {
        Some("Widevine")
    } else if s.contains("9a04f079-9840-4286-ab92-e65be0885f95") || s.contains("playready") {
        Some("PlayReady")
    } else if s.contains("94ce86fb-07ff-4f43-adb8-93d2fa968ca2") || s.contains("fairplay") {
        Some("FairPlay")
    } else {
        None
    }
}

/// `contentType`, else the major type of `mimeType`, lower-cased.
fn content_type(attrs: &[(String, String)]) -> Option<String> {
    attr(attrs, "contentType")
        .map(str::to_string)
        .or_else(|| {
            attr(attrs, "mimeType")
                .and_then(|m| m.split('/').next())
                .map(str::to_string)
        })
        .map(|s| s.to_ascii_lowercase())
}

/// A Representation's continuation across Periods is one track.
fn append_period(into: &mut Track, next: Track) {
    if next.init != into.init {
        if let Some(init) = next.init {
            into.init_changes.push((into.segments.len(), init));
        }
    }
    into.segments.extend(next.segments);
    into.numbers.extend(next.numbers);
    into.durations.extend(next.durations);
    if into.unsupported.is_none() {
        into.unsupported = next.unsupported;
    }
}

/// Parse an MPD into concrete tracks. `base` is the URL it was fetched from.
pub fn parse(text: &str, base: &str) -> Manifest {
    let mut mf = Manifest::default();
    let mut stack: Vec<Scope> = Vec::new();
    let mut cenc_only = false;
    // `<BaseURL>` text arrives after the element opens, so it lands here and
    // is attached to whichever scope was open.
    let mut want_base = false;
    let mut periods: Vec<Period> = Vec::new();
    // Representations in document order, resolved once every Period's
    // length is known.
    let mut reps: Vec<(Pending, Option<Template>)> = Vec::new();

    // Effective base URL: every BaseURL down the open scopes, joined.
    let effective_base = |stack: &Vec<Scope>| -> String {
        let mut url = base.to_string();
        for s in stack {
            if let Some(b) = &s.base {
                url = crate::url::join(&url, b).unwrap_or(url);
            }
        }
        url
    };
    let effective_template = |stack: &Vec<Scope>| -> Option<Template> {
        stack.iter().rev().find_map(|s| s.template.clone())
    };
    let effective_type = |stack: &Vec<Scope>| -> Option<String> {
        stack.iter().rev().find_map(|s| s.content_type.clone())
    };
    // The innermost open scope that carries addressing, for the children
    // (`S`, `SegmentURL`, `Initialization`) that fill it in.
    fn addressing(stack: &mut [Scope]) -> Option<&mut Template> {
        stack.iter_mut().rev().find_map(|s| s.template.as_mut())
    }

    let nodes = scan(text);
    for node in &nodes {
        let (name, attrs, is_empty) = match node {
            Node::Open(n, a) => (n.as_str(), a.as_slice(), false),
            Node::Empty(n, a) => (n.as_str(), a.as_slice(), true),
            Node::Close(n) => {
                want_base = false;
                // Elements that carry nothing inheritable (BaseURL,
                // ContentProtection, S) are never pushed, so their close tag
                // must not unwind the scopes that ARE open.
                if !stack.iter().any(|s| s.name == *n) {
                    continue;
                }
                while let Some(top) = stack.pop() {
                    let matched = top.name == *n;
                    // A Representation is finished now, so its own child
                    // SegmentTemplate (written back below on the way out of
                    // that child) is finally in hand.
                    if let Some(p) = &top.pending {
                        let template = top
                            .template
                            .clone()
                            .or_else(|| stack.iter().rev().find_map(|s| s.template.clone()));
                        // A Representation's own <BaseURL> is only known once
                        // it closes; fold it onto the base captured at the
                        // opening tag.
                        let p = match &top.base {
                            Some(b) => Pending {
                                base_url: crate::url::join(&p.base_url, b)
                                    .unwrap_or_else(|| p.base_url.clone()),
                                own_base: true,
                                ..p.clone()
                            },
                            None => p.clone(),
                        };
                        reps.push((p, template));
                    }
                    // A SegmentTimeline fills its SegmentTemplate only as its
                    // <S> children are read — after the parent scope already
                    // took a copy. Hand the finished template back on the way
                    // out, or the timeline is lost and the Representation
                    // sees an empty one.
                    if matches!(
                        top.name.as_str(),
                        "SegmentTemplate" | "SegmentList" | "SegmentBase"
                    ) {
                        if let (Some(t), Some(parent)) = (top.template.clone(), stack.last_mut()) {
                            parent.template = Some(t);
                        }
                    }
                    if matched {
                        break;
                    }
                }
                continue;
            }
            Node::Text(t) => {
                if want_base {
                    if let Some(top) = stack.last_mut() {
                        // Sibling BaseURLs are alternatives; the first is the
                        // default (ISO 23009-1 §5.6).
                        if top.base.is_none() {
                            top.base = Some(t.clone());
                        }
                    }
                    want_base = false;
                }
                continue;
            }
        };
        want_base = false;

        match name {
            "MPD" => {
                mf.live = attr(attrs, "type").is_some_and(|t| t.eq_ignore_ascii_case("dynamic"));
                mf.duration = attr(attrs, "mediaPresentationDuration")
                    .and_then(iso_duration)
                    .unwrap_or(0.0);
                mf.minimum_update_period =
                    attr(attrs, "minimumUpdatePeriod").and_then(iso_duration);
                stack.push(Scope {
                    name: name.into(),
                    ..Scope::default()
                });
            }
            "BaseURL" => {
                // The URL is this element's TEXT; the next Text node is it.
                want_base = !is_empty;
            }
            "Period" => {
                periods.push(Period {
                    start: attr(attrs, "start").and_then(iso_duration),
                    duration: attr(attrs, "duration").and_then(iso_duration),
                });
                if !is_empty {
                    stack.push(Scope {
                        name: name.into(),
                        ..Scope::default()
                    });
                }
            }
            "AdaptationSet" => {
                if !is_empty {
                    stack.push(Scope {
                        name: name.into(),
                        content_type: content_type(attrs),
                        ..Scope::default()
                    });
                }
            }
            "ContentProtection" => {
                let scheme = attr(attrs, "schemeIdUri").unwrap_or("");
                match drm_name(scheme) {
                    Some(n) => {
                        mf.drm.get_or_insert_with(|| n.to_string());
                    }
                    None if scheme.to_ascii_lowercase().contains("mp4protection") => {
                        cenc_only = true;
                    }
                    None => {}
                }
            }
            "SegmentTemplate" | "SegmentList" | "SegmentBase" => {
                let mut t = effective_template(&stack).unwrap_or_default();
                t.form = match name {
                    "SegmentList" => Addressing::List,
                    "SegmentBase" => Addressing::Base,
                    _ => Addressing::Template,
                };
                t.merge(attrs);
                if let Some(top) = stack.last_mut() {
                    top.template = Some(t.clone());
                }
                if !is_empty {
                    stack.push(Scope {
                        name: name.into(),
                        template: Some(t),
                        ..Scope::default()
                    });
                }
            }
            "Initialization" => {
                if let Some(t) = addressing(&mut stack) {
                    if let Some(u) = attr(attrs, "sourceURL") {
                        t.init = Some(u.to_string());
                    }
                    t.init_range = attr(attrs, "range").and_then(byte_range);
                }
            }
            "SegmentURL" => {
                if let Some(t) = addressing(&mut stack) {
                    if let Some(m) = attr(attrs, "media") {
                        t.list.push((
                            m.to_string(),
                            attr(attrs, "mediaRange").and_then(byte_range),
                        ));
                    }
                }
            }
            "S" => {
                if let Some(t) = addressing(&mut stack) {
                    if t.timeline.len() as u64 >= MAX_SEGMENTS {
                        continue;
                    }
                    t.timeline.push(TimelineEntry {
                        t: num(attrs, "t"),
                        d: num(attrs, "d").unwrap_or(0),
                        r: num(attrs, "r").unwrap_or(0),
                    });
                }
            }
            "Representation" => {
                let ctype = content_type(attrs).or_else(|| effective_type(&stack));
                let kind = match ctype.as_deref() {
                    Some("video") => TrackKind::Video,
                    Some("audio") => TrackKind::Audio,
                    // Subtitles, thumbnails and anything else are not media
                    // this can assemble.
                    _ => {
                        if !is_empty {
                            stack.push(Scope {
                                name: name.into(),
                                ..Scope::default()
                            });
                        }
                        continue;
                    }
                };
                let pending = Pending {
                    attrs: attrs.to_vec(),
                    kind,
                    base_url: effective_base(&stack),
                    own_base: false,
                    period: periods.len().saturating_sub(1),
                };
                if is_empty {
                    // Nothing can be nested in it, so the inherited template
                    // is the whole story.
                    reps.push((pending, effective_template(&stack)));
                } else {
                    stack.push(Scope {
                        name: name.into(),
                        pending: Some(pending),
                        ..Scope::default()
                    });
                }
            }
            _ => {
                if !is_empty {
                    stack.push(Scope {
                        name: name.into(),
                        ..Scope::default()
                    });
                }
            }
        }
    }

    let lengths = period_lengths(&periods, mf.duration);
    if mf.duration <= 0.0 && !mf.live {
        mf.duration = lengths.iter().sum();
    }
    // `(period, id)` of each track placed so far, in step with the lists.
    let mut placed_video: Vec<(usize, String)> = Vec::new();
    let mut placed_audio: Vec<(usize, String)> = Vec::new();
    for (p, template) in &reps {
        let seconds = lengths.get(p.period).copied().unwrap_or(mf.duration);
        let track = make_track(p, template.as_ref(), seconds);
        let (list, placed) = match p.kind {
            TrackKind::Video => (&mut mf.video, &mut placed_video),
            TrackKind::Audio => (&mut mf.audio, &mut placed_audio),
        };
        // The same Representation id in a LATER period continues the track.
        match placed
            .iter()
            .position(|(period, id)| *period < p.period && *id == track.id)
        {
            Some(i) => {
                placed[i].0 = p.period;
                append_period(&mut list[i], track);
            }
            None => {
                placed.push((p.period, track.id.clone()));
                list.push(track);
            }
        }
    }

    // Common encryption with no recognised system still needs a licence.
    if mf.drm.is_none() && cenc_only {
        mf.drm = Some("CENC".into());
    }
    let by_bandwidth =
        |a: &Track, b: &Track| b.bandwidth.unwrap_or(0).cmp(&a.bandwidth.unwrap_or(0));
    mf.video.sort_by(by_bandwidth);
    mf.audio.sort_by(by_bandwidth);
    mf
}

impl Manifest {
    /// How long to wait before re-reading a dynamic manifest. The clamp keeps
    /// a manifest declaring something absurd from either hammering the origin
    /// or falling behind its own sliding window.
    pub fn refresh_after(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(
            self.minimum_update_period.unwrap_or(3.0).clamp(1.0, 10.0),
        )
    }

    /// The window as `($Number$, segment, seconds)`, for a live recorder
    /// tracking what it has already taken and how much time that is.
    pub fn timed_window(track: &Track) -> Vec<(u64, Segment, f64)> {
        track
            .numbers
            .iter()
            .zip(&track.segments)
            .enumerate()
            .map(|(i, (n, s))| {
                (
                    *n,
                    s.clone(),
                    track.durations.get(i).copied().unwrap_or(0.0),
                )
            })
            .collect()
    }

    /// Pick the video rendition closest to what was asked for, by the same
    /// rule HLS uses: exact height, else the nearest at or below it.
    pub fn choose_video(&self, want_height: Option<u32>) -> Option<&Track> {
        if self.video.is_empty() {
            return None;
        }
        let Some(h) = want_height else {
            return self.video.first();
        };
        if let Some(t) = self.video.iter().find(|t| t.height == Some(h)) {
            return Some(t);
        }
        self.video
            .iter()
            .filter(|t| t.height.is_some_and(|th| th <= h))
            .max_by_key(|t| t.height.unwrap_or(0))
            .or_else(|| {
                self.video
                    .iter()
                    .min_by_key(|t| t.height.unwrap_or(u32::MAX))
            })
    }

    /// The best audio rendition, when the manifest carries audio separately.
    ///
    /// A Representation whose template resolved to nothing is not an audio
    /// track: recording it would produce an empty file and then fail the mux
    /// that tried to use it.
    pub fn choose_audio(&self) -> Option<&Track> {
        self.audio.iter().find(|t| !t.segments.is_empty())
    }

    /// Turn a chosen track into the same [`Plan`] the HLS path assembles.
    /// DASH is fragmented MP4, so init + fragments in order is already a
    /// playable file for each track on its own.
    pub fn plan(&self, track: &Track) -> Result<Plan, Refusal> {
        if let Some(d) = &self.drm {
            return Err(Refusal::Drm(d.clone()));
        }
        if self.live {
            return Err(Refusal::Live);
        }
        if track.segments.is_empty() {
            return Err(match &track.unsupported {
                Some(why) => Refusal::Unsupported(why.clone()),
                None => Refusal::Empty,
            });
        }
        Ok(Plan::assemble(
            track.init.clone(),
            track.init_changes.clone(),
            track.segments.clone(),
            Segments::Fmp4,
            None,
            self.duration,
            track.bandwidth,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape dash.akamaized.net serves, trimmed to three video
    /// renditions and its audio one.
    const BBB: &str = r#"<MPD mediaPresentationDuration="PT634.566S" type="static" xmlns="urn:mpeg:dash:schema:mpd:2011">
 <BaseURL>./</BaseURL>
 <Period>
  <AdaptationSet mimeType="video/mp4" contentType="video" par="16:9">
   <SegmentTemplate duration="120" timescale="30" media="$RepresentationID$/$RepresentationID$_$Number$.m4v" startNumber="1" initialization="$RepresentationID$/$RepresentationID$_0.m4v"/>
   <Representation id="bbb_30fps_320x180_200k" codecs="avc1.64000d" bandwidth="254320" width="320" height="180"/>
   <Representation id="bbb_30fps_1280x720_4000k" codecs="avc1.64001f" bandwidth="4952892" width="1280" height="720"/>
   <Representation id="bbb_30fps_1920x1080_8000k" codecs="avc1.640028" bandwidth="9914554" width="1920" height="1080"/>
  </AdaptationSet>
  <AdaptationSet mimeType="audio/mp4" contentType="audio">
   <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
   <SegmentTemplate duration="192512" timescale="48000" media="$RepresentationID$/$RepresentationID$_$Number$.m4a" startNumber="1" initialization="$RepresentationID$/$RepresentationID$_0.m4a"/>
   <Representation id="bbb_a64k" codecs="mp4a.40.5" bandwidth="67071" audioSamplingRate="48000">
    <AudioChannelConfiguration schemeIdUri="urn:mpeg:dash:23003:3:audio_channel_configuration:2011" value="2"/>
   </Representation>
  </AdaptationSet>
 </Period>
</MPD>"#;

    const BASE: &str = "https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd";

    #[test]
    fn the_akamai_reference_manifest_resolves_to_real_segment_urls() {
        let mf = parse(BBB, BASE);
        assert!(!mf.live);
        assert_eq!(mf.drm, None);
        assert_eq!(mf.duration, 634.566);
        assert_eq!(mf.video.len(), 3);
        assert_eq!(mf.audio.len(), 1);
        // Highest bandwidth first.
        assert_eq!(mf.video[0].height, Some(1080));

        let v = mf.choose_video(Some(720)).unwrap();
        assert_eq!(v.id, "bbb_30fps_1280x720_4000k");
        assert_eq!(
            v.init.as_ref().map(|s| s.url.as_str()),
            Some("https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps_1280x720_4000k/bbb_30fps_1280x720_4000k_0.m4v")
        );
        // 634.566 s at 120/30 = 4 s per segment -> 159 segments, numbered
        // from startNumber.
        assert_eq!(v.segments.len(), 159);
        assert_eq!(
            v.segments[0].url,
            "https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps_1280x720_4000k/bbb_30fps_1280x720_4000k_1.m4v"
        );
        assert_eq!(
            v.segments[158].url,
            "https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps_1280x720_4000k/bbb_30fps_1280x720_4000k_159.m4v"
        );

        let a = mf.choose_audio().unwrap();
        assert_eq!(a.id, "bbb_a64k");
        assert_eq!(a.segments.len(), 159); // 192512/48000 = 4.0107 s
        assert!(a.segments[0].url.ends_with("bbb_a64k/bbb_a64k_1.m4a"));
    }

    #[test]
    fn a_plan_is_fragmented_mp4_with_the_init_map_first() {
        let mf = parse(BBB, BASE);
        let plan = mf.plan(mf.choose_video(Some(1080)).unwrap()).unwrap();
        assert_eq!(plan.kind, Segments::Fmp4);
        assert!(plan.init.is_some());
        assert_eq!(plan.segments.len(), 159);
        assert_eq!(plan.native_ext(), "mp4");
        // 9914554 bps x 634.566 s / 8
        assert_eq!(plan.estimated_size, Some(786_429_859));
    }

    #[test]
    fn choosing_video_never_silently_steps_up() {
        let mf = parse(BBB, BASE);
        assert_eq!(mf.choose_video(Some(1080)).unwrap().height, Some(1080));
        assert_eq!(mf.choose_video(Some(900)).unwrap().height, Some(720));
        assert_eq!(mf.choose_video(Some(100)).unwrap().height, Some(180));
        assert_eq!(mf.choose_video(None).unwrap().height, Some(1080));
    }

    #[test]
    fn a_segment_template_nested_inside_a_representation_is_found() {
        // The shape live CDNs actually publish: the template is a CHILD of
        // the Representation, not a sibling on the AdaptationSet. Building
        // the track at the opening tag would see no template at all and
        // produce a rendition with zero segments.
        let mpd = r#"<MPD type="dynamic" minimumUpdatePeriod="PT3.5S" availabilityStartTime="2026-06-22T19:20:01Z">
          <Period id='1782156001' start="PT0S">
            <AdaptationSet id="1" mimeType="video/mp4" contentType="video">
              <Representation id="tracks-v1" width="1920" height="1080" bandwidth="2878000" codecs="avc1.4d4028">
                <SegmentTemplate initialization="tracks-v1/init.m4v" media="tracks-v1/seg-1782156001-$Number$.m4v?t=$Time$" timescale="1000" startNumber="1194974">
                  <SegmentTimeline><S t="5877243579" d="4999" r="3"/></SegmentTimeline>
                </SegmentTemplate>
              </Representation>
            </AdaptationSet>
            <AdaptationSet id="2" mimeType="audio/mp4" contentType="audio">
              <Representation id="tracks-a1" bandwidth="162000" codecs="mp4a.40.2">
                <AudioChannelConfiguration schemeIdUri="urn:mpeg:dash:23003:3:audio_channel_configuration:2011" value="6"/>
                <SegmentTemplate initialization="tracks-a1/init.m4v" media="tracks-a1/seg-1782156001-$Number$.m4v?t=$Time$" timescale="1000" startNumber="1194974"/>
              </Representation>
            </AdaptationSet>
          </Period></MPD>"#;
        let mf = parse(mpd, "https://cdn.example/live_cdn/abc/def/index.mpd");
        assert!(mf.live);
        assert_eq!(mf.minimum_update_period, Some(3.5));
        assert_eq!(mf.refresh_after(), std::time::Duration::from_secs_f64(3.5));

        let v = &mf.video[0];
        assert_eq!(v.id, "tracks-v1");
        assert_eq!(
            v.init.as_ref().map(|s| s.url.as_str()),
            Some("https://cdn.example/live_cdn/abc/def/tracks-v1/init.m4v")
        );
        // r="3" is three EXTRA repeats: four segments in the window.
        assert_eq!(v.segments.len(), 4, "the nested template was not applied");
        assert_eq!(v.numbers, [1194974, 1194975, 1194976, 1194977]);
        // Both $Number$ and $Time$ appear in the same media template.
        assert_eq!(
            v.segments[0].url,
            "https://cdn.example/live_cdn/abc/def/tracks-v1/seg-1782156001-1194974.m4v?t=5877243579"
        );
        assert_eq!(
            v.segments[1].url,
            "https://cdn.example/live_cdn/abc/def/tracks-v1/seg-1782156001-1194975.m4v?t=5877248578"
        );
        // The audio Representation has a child template too, and a nested
        // element before it must not hide it.
        assert_eq!(mf.audio.len(), 1);
        assert_eq!(mf.audio[0].id, "tracks-a1");
        assert!(mf.audio[0]
            .init
            .as_ref()
            .unwrap()
            .url
            .ends_with("tracks-a1/init.m4v"));
        // ...but this one's template resolved to no segments, so it is not
        // offered as a track to record: an empty file would only fail the mux.
        assert!(mf.audio[0].segments.is_empty());
        assert!(mf.choose_audio().is_none());
    }

    #[test]
    fn a_segment_timeline_drives_the_numbering_when_there_is_one() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT30S"><Period>
          <AdaptationSet contentType="video">
            <SegmentTemplate media="v/$Number%05d$.m4s" initialization="v/init.mp4" timescale="90000" startNumber="10">
              <SegmentTimeline>
                <S t="0" d="900000" r="2"/>
                <S d="450000"/>
              </SegmentTimeline>
            </SegmentTemplate>
            <Representation id="v0" bandwidth="1000" width="640" height="360"/>
          </AdaptationSet></Period></MPD>"#;
        let mf = parse(mpd, "https://e/x/m.mpd");
        let v = &mf.video[0];
        // 3 repeats of the first S plus the trailing one.
        assert_eq!(v.segments.len(), 4);
        // startNumber respected, and %05d zero-padded.
        assert_eq!(v.segments[0].url, "https://e/x/v/00010.m4s");
        assert_eq!(v.segments[3].url, "https://e/x/v/00013.m4s");
    }

    #[test]
    fn a_time_based_template_uses_the_timeline_start_times() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT12S"><Period>
          <AdaptationSet contentType="video">
            <SegmentTemplate media="v/$Time$.m4s" timescale="1000">
              <SegmentTimeline><S t="0" d="4000" r="2"/></SegmentTimeline>
            </SegmentTemplate>
            <Representation id="v0" bandwidth="1" width="10" height="10"/>
          </AdaptationSet></Period></MPD>"#;
        let mf = parse(mpd, "https://e/m.mpd");
        assert_eq!(
            mf.video[0]
                .segments
                .iter()
                .map(|s| s.url.as_str())
                .collect::<Vec<_>>(),
            [
                "https://e/v/0.m4s",
                "https://e/v/4000.m4s",
                "https://e/v/8000.m4s"
            ]
        );
    }

    #[test]
    fn a_representations_own_base_url_is_applied() {
        // A BaseURL inside the Representation is legal and some packagers
        // emit one. Like its SegmentTemplate it is only visible once the
        // element closes, so building the track at the opening tag would
        // resolve every segment against the AdaptationSet's base instead.
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT8S">
          <BaseURL>https://cdn.example/v1/</BaseURL><Period>
          <AdaptationSet contentType="video"><BaseURL>video/</BaseURL>
            <SegmentTemplate media="$Number$.m4s" duration="4" timescale="1" startNumber="1" initialization="i.mp4"/>
            <Representation id="v0" bandwidth="1" width="10" height="10">
              <BaseURL>hi/</BaseURL>
            </Representation>
          </AdaptationSet></Period></MPD>"#;
        let mf = parse(mpd, "https://origin.example/a/b.mpd");
        assert_eq!(
            mf.video[0].init.as_ref().map(|s| s.url.as_str()),
            Some("https://cdn.example/v1/video/hi/i.mp4")
        );
        assert_eq!(
            mf.video[0].segments[0].url,
            "https://cdn.example/v1/video/hi/1.m4s"
        );
    }

    #[test]
    fn base_urls_nest_from_the_manifest_down() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT8S">
          <BaseURL>https://cdn.example/v1/</BaseURL><Period>
          <AdaptationSet contentType="video"><BaseURL>video/</BaseURL>
            <SegmentTemplate media="$Number$.m4s" duration="4" timescale="1" startNumber="1" initialization="i.mp4"/>
            <Representation id="v0" bandwidth="1" width="10" height="10"/>
          </AdaptationSet></Period></MPD>"#;
        let mf = parse(mpd, "https://origin.example/a/b.mpd");
        assert_eq!(
            mf.video[0].init.as_ref().map(|s| s.url.as_str()),
            Some("https://cdn.example/v1/video/i.mp4")
        );
        assert_eq!(
            mf.video[0].segments[0].url,
            "https://cdn.example/v1/video/1.m4s"
        );
    }

    #[test]
    fn drm_is_named_and_refused() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT8S"><Period>
          <AdaptationSet contentType="video">
            <ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"/>
            <SegmentTemplate media="$Number$.m4s" duration="4" timescale="1" startNumber="1"/>
            <Representation id="v0" bandwidth="1" width="10" height="10"/>
          </AdaptationSet></Period></MPD>"#;
        let mf = parse(mpd, "https://e/m.mpd");
        assert_eq!(mf.drm.as_deref(), Some("Widevine"));
        assert_eq!(mf.plan(&mf.video[0]), Err(Refusal::Drm("Widevine".into())));
    }

    #[test]
    fn cenc_alone_is_still_refused() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT8S"><Period>
          <AdaptationSet contentType="video">
            <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011" value="cenc"/>
            <SegmentTemplate media="$Number$.m4s" duration="4" timescale="1" startNumber="1"/>
            <Representation id="v0" bandwidth="1" width="10" height="10"/>
          </AdaptationSet></Period></MPD>"#;
        assert_eq!(parse(mpd, "https://e/m.mpd").drm.as_deref(), Some("CENC"));
    }

    #[test]
    fn a_dynamic_manifest_is_live_and_refused_by_the_vod_path() {
        let mpd = r#"<MPD type="dynamic"><Period><AdaptationSet contentType="video">
          <SegmentTemplate media="$Number$.m4s" duration="4" timescale="1" startNumber="1"/>
          <Representation id="v0" bandwidth="1" width="10" height="10"/>
          </AdaptationSet></Period></MPD>"#;
        let mf = parse(mpd, "https://e/m.mpd");
        assert!(mf.live);
        assert_eq!(mf.plan(&mf.video[0]), Err(Refusal::Live));
    }

    #[test]
    fn subtitle_and_image_tracks_are_not_offered_as_media() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT8S"><Period>
          <AdaptationSet contentType="text"><Representation id="t0" bandwidth="100"/></AdaptationSet>
          <AdaptationSet mimeType="image/jpeg"><Representation id="i0" bandwidth="100"/></AdaptationSet>
          </Period></MPD>"#;
        let mf = parse(mpd, "https://e/m.mpd");
        assert!(mf.video.is_empty() && mf.audio.is_empty());
    }

    #[test]
    fn namespaced_elements_and_escaped_attributes_are_understood() {
        let mpd = r#"<?xml version="1.0"?><!-- a comment -->
          <dash:MPD xmlns:dash="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S">
          <dash:Period><dash:AdaptationSet contentType="video">
          <dash:SegmentTemplate media="s.m4s?a=1&amp;b=2" duration="4" timescale="1" startNumber="1"/>
          <dash:Representation id="v0" bandwidth="1" width="10" height="10"/>
          </dash:AdaptationSet></dash:Period></dash:MPD>"#;
        let mf = parse(mpd, "https://e/m.mpd");
        assert_eq!(mf.video.len(), 1);
        assert_eq!(mf.video[0].segments[0].url, "https://e/s.m4s?a=1&b=2");
    }

    #[test]
    fn template_expansion_handles_widths_and_escaped_dollars() {
        assert_eq!(expand("$Number$", "id", 7, 0, 0), "7");
        assert_eq!(expand("$Number%05d$", "id", 7, 0, 0), "00007");
        assert_eq!(expand("$RepresentationID$_$Number$", "v1", 3, 0, 0), "v1_3");
        assert_eq!(expand("$Time$", "id", 0, 9000, 0), "9000");
        assert_eq!(expand("$Bandwidth$", "id", 0, 0, 128000), "128000");
        assert_eq!(expand("a$$b", "id", 0, 0, 0), "a$b");
        // An unknown token is left alone rather than dropped.
        assert_eq!(expand("$Nope$", "id", 0, 0, 0), "$Nope$");
    }

    #[test]
    fn iso_durations_cover_the_forms_manifests_use() {
        assert_eq!(iso_duration("PT634.566S"), Some(634.566));
        assert_eq!(iso_duration("PT1H2M3S"), Some(3723.0));
        assert_eq!(iso_duration("PT2M"), Some(120.0));
        assert_eq!(iso_duration("P1DT1S"), Some(86401.0));
        assert_eq!(iso_duration("nonsense"), None);
    }

    #[test]
    fn start_number_zero_is_respected_and_only_an_absent_one_defaults() {
        let mpd = |attr: &str| {
            format!(
                r#"<MPD type="static" mediaPresentationDuration="PT8S"><Period>
                  <AdaptationSet contentType="video">
                    <SegmentTemplate media="$Number$.m4s" duration="4" timescale="1" {attr}/>
                    <Representation id="v0" bandwidth="1" width="10" height="10"/>
                  </AdaptationSet></Period></MPD>"#
            )
        };
        let zero = parse(&mpd(r#"startNumber="0""#), "https://e/m.mpd");
        assert_eq!(zero.video[0].numbers, [0, 1]);
        assert_eq!(zero.video[0].segments[0].url, "https://e/0.m4s");
        let absent = parse(&mpd(""), "https://e/m.mpd");
        assert_eq!(absent.video[0].numbers, [1, 2]);
    }

    #[test]
    fn a_segment_list_names_each_segment_and_its_byte_range() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT9S"><Period>
          <AdaptationSet contentType="video">
            <Representation id="v0" bandwidth="1" width="10" height="10">
              <BaseURL>all.mp4</BaseURL>
              <SegmentList timescale="1000" duration="3000">
                <Initialization sourceURL="all.mp4" range="0-863"/>
                <SegmentURL media="all.mp4" mediaRange="864-5000"/>
                <SegmentURL media="all.mp4" mediaRange="5001-9000"/>
                <SegmentURL media="tail.m4s"/>
              </SegmentList>
            </Representation>
          </AdaptationSet></Period></MPD>"#;
        let mf = parse(mpd, "https://e/d/m.mpd");
        let v = &mf.video[0];
        assert_eq!(v.unsupported, None);
        assert_eq!(
            v.init,
            Some(Segment {
                url: "https://e/d/all.mp4".into(),
                range: Some((0, 864)),
                key: None
            })
        );
        assert_eq!(v.segments.len(), 3);
        assert_eq!(v.segments[0].range, Some((864, 5000 - 864 + 1)));
        assert_eq!(v.segments[1].range, Some((5001, 4000)));
        assert_eq!(v.segments[2], Segment::new("https://e/d/tail.m4s"));
        assert_eq!(v.numbers, [1, 2, 3]);
        assert_eq!(v.durations, [3.0, 3.0, 3.0]);
        let plan = mf.plan(v).unwrap();
        assert_eq!(
            plan.segments[0].range_header().as_deref(),
            Some("bytes=864-5000")
        );

        // Without a duration the period's length is shared out evenly, and a
        // list that names nothing is refused by name.
        let bare = mpd
            .replace(r#"timescale="1000" duration="3000""#, "")
            .replace(r#"<SegmentURL media="tail.m4s"/>"#, "");
        let mf = parse(&bare, "https://e/d/m.mpd");
        assert_eq!(mf.video[0].durations, [4.5, 4.5]);
        let empty = mpd.replace(
            r#"<SegmentURL media="all.mp4" mediaRange="864-5000"/>
                <SegmentURL media="all.mp4" mediaRange="5001-9000"/>
                <SegmentURL media="tail.m4s"/>"#,
            "",
        );
        let mf = parse(&empty, "https://e/d/m.mpd");
        assert!(matches!(
            mf.plan(&mf.video[0]),
            Err(Refusal::Unsupported(w)) if w.contains("SegmentURL")
        ));
    }

    #[test]
    fn a_segment_base_is_one_whole_file_carrying_its_own_init() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT30S"><Period>
          <AdaptationSet contentType="video">
            <Representation id="v0" bandwidth="1" width="10" height="10">
              <BaseURL>movie.mp4</BaseURL>
              <SegmentBase indexRange="864-1500"><Initialization range="0-863"/></SegmentBase>
            </Representation>
          </AdaptationSet>
          <AdaptationSet contentType="audio">
            <Representation id="a0" bandwidth="1"><BaseURL>sound.mp4</BaseURL></Representation>
          </AdaptationSet></Period></MPD>"#;
        let mf = parse(mpd, "https://e/d/m.mpd");
        let v = &mf.video[0];
        // The init lives inside the same file, so fetching it separately
        // would write those bytes twice.
        assert_eq!(v.init, None);
        assert_eq!(v.segments, [Segment::new("https://e/d/movie.mp4")]);
        assert_eq!(v.durations, [30.0]);
        assert!(mf.plan(v).is_ok());
        // No addressing at all plus its own BaseURL is the same thing.
        assert_eq!(
            mf.choose_audio().unwrap().segments,
            [Segment::new("https://e/d/sound.mp4")]
        );
    }

    #[test]
    fn a_representation_with_no_addressing_is_refused_by_name() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT8S"><Period>
          <AdaptationSet contentType="video">
            <Representation id="v0" bandwidth="1" width="10" height="10"/>
          </AdaptationSet></Period></MPD>"#;
        let mf = parse(mpd, "https://e/m.mpd");
        let Err(Refusal::Unsupported(why)) = mf.plan(&mf.video[0]) else {
            panic!("a representation with nothing to fetch must say so");
        };
        assert!(why.contains("no segment addressing"), "{why}");
        assert!(why.contains("v0"), "{why}");
    }

    #[test]
    fn periods_sharing_a_representation_id_continue_one_track_in_order() {
        // Period one has no duration; its length is the gap to period two's
        // start. Period two runs to the end of the presentation.
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT18S">
          <Period id="p1" start="PT0S">
            <AdaptationSet contentType="video">
              <SegmentTemplate media="p1/$Number$.m4s" initialization="p1/init.mp4" duration="2" timescale="1" startNumber="1"/>
              <Representation id="v0" bandwidth="1" width="10" height="10"/>
            </AdaptationSet>
          </Period>
          <Period id="p2" start="PT10S">
            <AdaptationSet contentType="video">
              <SegmentTemplate media="p2/$Number$.m4s" initialization="p2/init.mp4" duration="2" timescale="1" startNumber="1"/>
              <Representation id="v0" bandwidth="1" width="10" height="10"/>
              <Representation id="v1" bandwidth="2" width="20" height="20"/>
            </AdaptationSet>
          </Period></MPD>"#;
        let mf = parse(mpd, "https://e/m.mpd");
        // v0 spans both periods; v1 exists only in the second.
        assert_eq!(mf.video.len(), 2);
        let v0 = mf.video.iter().find(|t| t.id == "v0").unwrap();
        assert_eq!(v0.segments.len(), 5 + 4, "10 s then 8 s of 2 s segments");
        assert!(v0.segments[4].url.ends_with("p1/5.m4s"));
        assert!(v0.segments[5].url.ends_with("p2/1.m4s"));
        assert_eq!(
            v0.init.as_ref().map(|s| s.url.as_str()),
            Some("https://e/p1/init.mp4")
        );
        assert_eq!(
            v0.init_changes,
            [(5, Segment::new("https://e/p2/init.mp4"))],
            "the second period's init must be written before its first segment"
        );
        let plan = mf.plan(v0).unwrap();
        assert_eq!(plan.init_changes.len(), 1);
        assert_eq!(plan.segments.len(), 9);
        let v1 = mf.video.iter().find(|t| t.id == "v1").unwrap();
        assert_eq!(v1.segments.len(), 4);
        assert!(v1.init_changes.is_empty());
    }

    #[test]
    fn an_unbounded_timeline_is_refused_rather_than_expanded() {
        let repeat = r#"<MPD type="static" mediaPresentationDuration="PT8S"><Period>
          <AdaptationSet contentType="video">
            <SegmentTemplate media="$Number$.m4s" timescale="1">
              <SegmentTimeline><S t="0" d="1" r="4000000000"/></SegmentTimeline>
            </SegmentTemplate>
            <Representation id="v0" bandwidth="1" width="10" height="10"/>
          </AdaptationSet></Period></MPD>"#;
        let mf = parse(repeat, "https://e/m.mpd");
        assert!(mf.video[0].segments.is_empty());
        let Err(Refusal::Unsupported(why)) = mf.plan(&mf.video[0]) else {
            panic!("four billion segments must be refused");
        };
        assert!(why.contains("1000000"), "{why}");

        // A microsecond duration over an hour is the same problem spelled
        // with arithmetic instead of a repeat count.
        let tiny = r#"<MPD type="static" mediaPresentationDuration="PT1H"><Period>
          <AdaptationSet contentType="video">
            <SegmentTemplate media="$Number$.m4s" duration="1" timescale="1000000"/>
            <Representation id="v0" bandwidth="1" width="10" height="10"/>
          </AdaptationSet></Period></MPD>"#;
        let mf = parse(tiny, "https://e/m.mpd");
        assert!(matches!(
            mf.plan(&mf.video[0]),
            Err(Refusal::Unsupported(_))
        ));
    }

    #[test]
    fn base_url_text_is_unescaped_and_the_first_sibling_is_the_default() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT4S">
          <BaseURL>https://a.example/v/a&amp;b/</BaseURL>
          <BaseURL>https://b.example/v/</BaseURL>
          <Period><AdaptationSet contentType="video">
            <SegmentTemplate media="s$Number$.m4s" duration="4" timescale="1"/>
            <Representation id="v0" bandwidth="1" width="10" height="10"/>
          </AdaptationSet></Period></MPD>"#;
        let mf = parse(mpd, "https://origin.example/m.mpd");
        assert_eq!(
            mf.video[0].segments[0].url,
            "https://a.example/v/a&b/s1.m4s"
        );
    }

    #[test]
    fn time_in_a_duration_only_template_advances_from_the_presentation_offset() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT12S"><Period>
          <AdaptationSet contentType="video">
            <SegmentTemplate media="v/$Time$.m4s" duration="4000" timescale="1000" presentationTimeOffset="100"/>
            <Representation id="v0" bandwidth="1" width="10" height="10"/>
          </AdaptationSet></Period></MPD>"#;
        let mf = parse(mpd, "https://e/m.mpd");
        assert_eq!(
            mf.video[0]
                .segments
                .iter()
                .map(|s| s.url.as_str())
                .collect::<Vec<_>>(),
            [
                "https://e/v/100.m4s",
                "https://e/v/4100.m4s",
                "https://e/v/8100.m4s"
            ]
        );
        // A timeline's `t` values already include the offset and are used as
        // written.
        let timeline = r#"<MPD type="static" mediaPresentationDuration="PT8S"><Period>
          <AdaptationSet contentType="video">
            <SegmentTemplate media="v/$Time$.m4s" timescale="1000" presentationTimeOffset="100">
              <SegmentTimeline><S t="100" d="4000" r="1"/></SegmentTimeline>
            </SegmentTemplate>
            <Representation id="v0" bandwidth="1" width="10" height="10"/>
          </AdaptationSet></Period></MPD>"#;
        let mf = parse(timeline, "https://e/m.mpd");
        assert_eq!(mf.video[0].segments[1].url, "https://e/v/4100.m4s");
    }

    #[test]
    fn a_negative_repeat_runs_to_the_end_of_the_period() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT14S"><Period>
          <AdaptationSet contentType="video">
            <SegmentTemplate media="v/$Number$.m4s" timescale="1000">
              <SegmentTimeline><S t="0" d="4000" r="-1"/></SegmentTimeline>
            </SegmentTemplate>
            <Representation id="v0" bandwidth="1" width="10" height="10"/>
          </AdaptationSet></Period></MPD>"#;
        let mf = parse(mpd, "https://e/m.mpd");
        assert_eq!(mf.video[0].numbers, [1, 2, 3, 4], "14 s of 4 s segments");
        // With no period length to run to, it is one segment, not a guess.
        let live = mpd.replace(
            r#"type="static" mediaPresentationDuration="PT14S""#,
            r#"type="dynamic""#,
        );
        assert_eq!(parse(&live, "https://e/m.mpd").video[0].numbers, [1]);
    }

    #[test]
    fn absurd_timeline_values_saturate_instead_of_overflowing() {
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT8S"><Period>
          <AdaptationSet contentType="video">
            <SegmentTemplate media="v/$Number$-$Time$.m4s" timescale="1" startNumber="18446744073709551615">
              <SegmentTimeline><S t="18446744073709551610" d="10" r="2"/><S d="18446744073709551615"/></SegmentTimeline>
            </SegmentTemplate>
            <Representation id="v0" bandwidth="1" width="10" height="10"/>
          </AdaptationSet></Period></MPD>"#;
        let mf = parse(mpd, "https://e/m.mpd");
        assert_eq!(mf.video[0].segments.len(), 4);
        assert!(mf.video[0].numbers.iter().all(|&n| n == u64::MAX));
    }
}
