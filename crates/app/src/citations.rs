//! Citations in a lesson: their order, how their place in a source reads ("p. 3",
//! "1:05–1:20"), and what the source viewer shows for one. Pure data: no Dioxus here, so
//! it is unit-tested directly.

use margin_shared::{Citation, SourceKind};

/// The lesson's citations by number, each number once (the first wins).
pub fn ordered(citations: &[Citation]) -> Vec<Citation> {
    let mut out: Vec<Citation> = Vec::with_capacity(citations.len());
    for c in citations {
        if !out.iter().any(|o| o.n == c.n) {
            out.push(c.clone());
        }
    }
    out.sort_by_key(|c| c.n);
    out
}

/// "0:07", "1:05", "1:02:05".
pub fn clock(ms: u64) -> String {
    let s = ms / 1000;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Where in its source a citation points: "p. 3", "1:05–1:20", "1:05"; `None` when the
/// whole source is meant (an image, a text passage).
pub fn locator(c: &Citation) -> Option<String> {
    if let Some(page) = c.page {
        return Some(format!("p. {page}"));
    }
    match (c.start_ms, c.end_ms) {
        (Some(start), Some(end)) if end > start => {
            Some(format!("{}–{}", clock(start), clock(end)))
        }
        (Some(start), _) => Some(clock(start)),
        _ => None,
    }
}

pub fn kind_label(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Text => "Text",
        SourceKind::Code => "Code",
        SourceKind::Pdf => "PDF",
        SourceKind::Image => "Image",
        SourceKind::Audio => "Audio",
        SourceKind::Video => "Video",
    }
}

/// The accessible name of a citation marker: "Source 2: lecture-12.m4a".
pub fn marker_label(n: u32, source_name: &str) -> String {
    format!("Source {n}: {source_name}")
}

/// The citations before and after `n` in `ordered`, for the viewer's Previous and Next.
pub fn neighbors(ordered: &[Citation], n: u32) -> (Option<u32>, Option<u32>) {
    let Some(i) = ordered.iter().position(|c| c.n == n) else {
        return (None, None);
    };
    let prev = i.checked_sub(1).map(|j| ordered[j].n);
    let next = ordered.get(i + 1).map(|c| c.n);
    (prev, next)
}

/// `n`'s place in the list: (1-based position, count).
pub fn position(ordered: &[Citation], n: u32) -> Option<(usize, usize)> {
    ordered
        .iter()
        .position(|c| c.n == n)
        .map(|i| (i + 1, ordered.len()))
}

/// Seconds for a media element's `currentTime`.
pub fn seconds(ms: u64) -> f64 {
    ms as f64 / 1000.0
}

/// A media URL that starts playing at `start_ms` (a W3C media fragment, `#t=`). The
/// fragment never reaches the service, so a signed query string stays valid.
pub fn media_at(url: &str, start_ms: Option<u64>) -> String {
    let base = url.split('#').next().unwrap_or(url);
    match start_ms {
        Some(ms) if ms > 0 => format!("{base}#t={}", trim_float(seconds(ms))),
        _ => base.to_string(),
    }
}

fn trim_float(v: f64) -> String {
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The URL that opens the whole source in the browser: at the cited page for a PDF, at
/// the cited time for audio and video. `None` when the service offers no original.
pub fn open_url(c: &Citation, media_url: Option<&str>) -> Option<String> {
    let url = media_url?;
    let base = url.split('#').next().unwrap_or(url);
    Some(match (c.kind, c.page) {
        (SourceKind::Pdf, Some(page)) => format!("{base}#page={page}"),
        (SourceKind::Audio | SourceKind::Video, _) => media_at(base, c.start_ms),
        _ => base.to_string(),
    })
}

/// "Open PDF", "Open video"…: the label of the viewer's button for `open_url`.
pub fn open_label(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Pdf => "Open PDF",
        SourceKind::Image => "Open image",
        SourceKind::Audio => "Open audio",
        SourceKind::Video => "Open video",
        SourceKind::Text | SourceKind::Code => "Open file",
    }
}

/// A timed passage of an audio or video source, as the viewer's transcript lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// The citation it comes from.
    pub n: u32,
    pub start_ms: u64,
    pub end_ms: Option<u64>,
    pub text: String,
    /// The passage the open citation points at.
    pub current: bool,
}

/// What the source viewer shows for a citation.
#[derive(Debug, Clone, PartialEq)]
pub enum Viewer {
    /// An image file, full size (fit to the dialog).
    Image { url: String },
    /// A PDF page: its rendered image when the service has one, and the page's text.
    PdfPage {
        page: Option<u32>,
        image: Option<String>,
        excerpt: String,
    },
    /// An audio clip: the player from `start_ms`, and the source's cited passages with
    /// this one marked.
    Audio {
        src: Option<String>,
        start_ms: u64,
        transcript: Vec<Segment>,
    },
    /// A video from the frame's time; the frame is the poster until it plays.
    Video {
        src: Option<String>,
        start_ms: u64,
        poster: Option<String>,
        transcript: Vec<Segment>,
    },
    /// A text or code passage.
    Passage { excerpt: String, code: bool },
    /// Nothing to show: the source has no picture and the passage no text.
    Missing,
}

/// What to show for `c`, given the lesson's citations (for an audio or video source's
/// other passages) and the source's `media_url` (`None` when the source is gone or the
/// service offers no original).
pub fn viewer(c: &Citation, all: &[Citation], media_url: Option<&str>) -> Viewer {
    let excerpt = c.excerpt.trim().to_string();
    match c.kind {
        SourceKind::Image => match c.image_url.as_deref().or(media_url) {
            Some(url) => Viewer::Image {
                url: url.to_string(),
            },
            None if !excerpt.is_empty() => Viewer::Passage {
                excerpt,
                code: false,
            },
            None => Viewer::Missing,
        },
        SourceKind::Pdf => Viewer::PdfPage {
            page: c.page,
            image: c.image_url.clone(),
            excerpt,
        },
        SourceKind::Audio => Viewer::Audio {
            src: media_url.map(str::to_string),
            start_ms: c.start_ms.unwrap_or(0),
            transcript: transcript(c, all),
        },
        SourceKind::Video => Viewer::Video {
            src: media_url.map(str::to_string),
            start_ms: c.start_ms.unwrap_or(0),
            poster: c.image_url.clone(),
            transcript: transcript(c, all),
        },
        SourceKind::Text | SourceKind::Code if excerpt.is_empty() => Viewer::Missing,
        SourceKind::Text | SourceKind::Code => Viewer::Passage {
            excerpt,
            code: c.kind == SourceKind::Code,
        },
    }
}

/// The timed passages of `c`'s source that the lesson cites, in time order, with `c`'s
/// marked. Passages without text (bare video frames) are left out; one cited twice is
/// listed once.
pub fn transcript(c: &Citation, all: &[Citation]) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for other in all.iter().filter(|o| o.source_id == c.source_id) {
        let (Some(start_ms), text) = (other.start_ms, other.excerpt.trim()) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let current = other.n == c.n;
        if let Some(seen) = out
            .iter_mut()
            .find(|s| s.start_ms == start_ms && s.text == text)
        {
            seen.current |= current;
            continue;
        }
        out.push(Segment {
            n: other.n,
            start_ms,
            end_ms: other.end_ms,
            text: text.to_string(),
            current,
        });
    }
    out.sort_by_key(|s| (s.start_ms, s.n));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cite(n: u32, source: &str, kind: SourceKind) -> Citation {
        Citation {
            n,
            source_id: source.into(),
            source_name: format!("{source} file"),
            kind,
            page: None,
            start_ms: None,
            end_ms: None,
            excerpt: format!("passage {n}"),
            image_url: None,
        }
    }

    #[test]
    fn citations_are_listed_by_number_once() {
        let list = vec![
            cite(3, "a", SourceKind::Text),
            cite(1, "b", SourceKind::Pdf),
            cite(2, "c", SourceKind::Code),
            Citation {
                excerpt: "a duplicate".into(),
                ..cite(1, "z", SourceKind::Text)
            },
        ];
        let ordered = ordered(&list);
        let ns: Vec<u32> = ordered.iter().map(|c| c.n).collect();
        assert_eq!(ns, vec![1, 2, 3]);
        assert_eq!(ordered[0].source_id, "b");
        assert_eq!(neighbors(&ordered, 1), (None, Some(2)));
        assert_eq!(neighbors(&ordered, 2), (Some(1), Some(3)));
        assert_eq!(neighbors(&ordered, 3), (Some(2), None));
        assert_eq!(neighbors(&ordered, 9), (None, None));
        assert_eq!(position(&ordered, 2), Some((2, 3)));
        assert_eq!(position(&ordered, 9), None);
    }

    #[test]
    fn locators_read_as_pages_and_times() {
        assert_eq!(clock(7_400), "0:07");
        assert_eq!(clock(65_000), "1:05");
        assert_eq!(clock(3_725_000), "1:02:05");
        let page = Citation {
            page: Some(3),
            ..cite(1, "a", SourceKind::Pdf)
        };
        assert_eq!(locator(&page).as_deref(), Some("p. 3"));
        let clip = Citation {
            start_ms: Some(65_000),
            end_ms: Some(80_500),
            ..cite(2, "b", SourceKind::Audio)
        };
        assert_eq!(locator(&clip).as_deref(), Some("1:05–1:20"));
        let frame = Citation {
            start_ms: Some(4_000),
            ..cite(3, "c", SourceKind::Video)
        };
        assert_eq!(locator(&frame).as_deref(), Some("0:04"));
        assert_eq!(locator(&cite(4, "d", SourceKind::Image)), None);
        assert_eq!(marker_label(2, "talk.m4a"), "Source 2: talk.m4a");
    }

    #[test]
    fn media_urls_start_at_the_cited_time_and_page() {
        let signed = "http://s/media/sources/a/original?exp=1&sig=ab";
        assert_eq!(
            media_at(signed, Some(65_500)),
            format!("{signed}#t=65.5")
        );
        assert_eq!(media_at(signed, Some(0)), signed);
        assert_eq!(media_at(&format!("{signed}#t=3"), None), signed);
        let page = Citation {
            page: Some(3),
            ..cite(1, "a", SourceKind::Pdf)
        };
        assert_eq!(
            open_url(&page, Some(signed)),
            Some(format!("{signed}#page=3"))
        );
        let clip = Citation {
            start_ms: Some(12_000),
            ..cite(2, "b", SourceKind::Video)
        };
        assert_eq!(open_url(&clip, Some(signed)), Some(format!("{signed}#t=12")));
        assert_eq!(
            open_url(&cite(3, "c", SourceKind::Text), Some(signed)),
            Some(signed.to_string())
        );
        assert_eq!(open_url(&clip, None), None);
    }

    #[test]
    fn the_viewer_shows_each_kind() {
        let image = Citation {
            image_url: Some("img".into()),
            excerpt: String::new(),
            ..cite(1, "i", SourceKind::Image)
        };
        assert_eq!(
            viewer(&image, &[], Some("orig")),
            Viewer::Image { url: "img".into() }
        );
        // Without a picture of its own, an image shows the original.
        let bare = Citation {
            excerpt: String::new(),
            ..cite(1, "i", SourceKind::Image)
        };
        assert_eq!(
            viewer(&bare, &[], Some("orig")),
            Viewer::Image { url: "orig".into() }
        );
        assert_eq!(viewer(&bare, &[], None), Viewer::Missing);

        let page = Citation {
            page: Some(4),
            image_url: Some("page-4.png".into()),
            ..cite(2, "p", SourceKind::Pdf)
        };
        assert_eq!(
            viewer(&page, &[], Some("orig")),
            Viewer::PdfPage {
                page: Some(4),
                image: Some("page-4.png".into()),
                excerpt: "passage 2".into()
            }
        );

        let code = cite(3, "c", SourceKind::Code);
        assert_eq!(
            viewer(&code, &[], None),
            Viewer::Passage {
                excerpt: "passage 3".into(),
                code: true
            }
        );
        let empty = Citation {
            excerpt: "  ".into(),
            ..cite(4, "t", SourceKind::Text)
        };
        assert_eq!(viewer(&empty, &[], None), Viewer::Missing);
    }

    #[test]
    fn audio_and_video_list_the_sources_cited_passages() {
        let clip = |n, start, text: &str| Citation {
            start_ms: Some(start),
            end_ms: Some(start + 5_000),
            excerpt: text.into(),
            ..cite(n, "talk", SourceKind::Audio)
        };
        let all = vec![
            clip(5, 40_000, "Later on."),
            clip(2, 10_000, "The overshoot stays."),
            cite(3, "other", SourceKind::Text),
            clip(6, 10_000, "The overshoot stays."),
            clip(7, 20_000, ""),
        ];
        let Viewer::Audio {
            src,
            start_ms,
            transcript,
        } = viewer(&all[3], &all, Some("http://s/a.m4a"))
        else {
            panic!("not audio");
        };
        assert_eq!(src.as_deref(), Some("http://s/a.m4a"));
        assert_eq!(start_ms, 10_000);
        let texts: Vec<(&str, bool)> = transcript
            .iter()
            .map(|s| (s.text.as_str(), s.current))
            .collect();
        assert_eq!(
            texts,
            vec![("The overshoot stays.", true), ("Later on.", false)]
        );
        assert_eq!(transcript[0].n, 2);

        let frame = Citation {
            start_ms: Some(4_000),
            image_url: Some("frame-4000.jpg".into()),
            excerpt: String::new(),
            ..cite(1, "film", SourceKind::Video)
        };
        assert_eq!(
            viewer(&frame, std::slice::from_ref(&frame), None),
            Viewer::Video {
                src: None,
                start_ms: 4_000,
                poster: Some("frame-4000.jpg".into()),
                transcript: Vec::new(),
            }
        );
    }
}
