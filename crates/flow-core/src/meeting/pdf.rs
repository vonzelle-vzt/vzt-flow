//! Small, offline PDF 1.4 exporter. Helvetica uses Windows-1252 (WinAnsi);
//! the Markdown transcript remains the lossless original for unsupported text.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct PdfOptions {
    pub dir: PathBuf,
    pub enabled: bool,
}

pub struct MeetingDoc {
    pub title: String,
    pub date_line: String,
    pub duration: String,
    pub summary_md: Option<String>,
    pub notes: Option<String>,
    pub transcript_lines: Vec<String>,
    pub coverage_note: Option<String>,
    pub source_path: PathBuf,
}

// The defined Windows-1252 characters in the 0x80..0x9f range. Control
// positions are deliberately absent, rather than emitted as invisible glyphs.
const SPECIAL: &str = "€\u{0081}‚ƒ„…†‡ˆ‰Š‹Œ\u{008d}Ž\u{008f}\u{0090}‘’“”•–—˜™š›œ\u{009d}žŸ";
fn winansi(c: char) -> Option<u8> {
    match c {
        ' '..='~' | '\u{00a0}'..='\u{00ff}' => Some(c as u8),
        _ if !c.is_control() => SPECIAL.chars().position(|v| v == c).map(|i| 128 + i as u8),
        _ => None,
    }
}

/// Return printable WinAnsi text as Unicode, plus the number of lost glyphs.
/// Encoding to actual WinAnsi bytes happens only when writing PDF strings.
pub fn sanitize_winansi(s: &str) -> (String, usize) {
    let mut out = String::new();
    let mut count = 0;
    for c in s.chars() {
        match c {
            '\u{200b}'..='\u{200f}' | '\u{2060}' | '\u{feff}' => {}
            '→' => out.push_str("->"),
            '\n' | '\r' => out.push(c),
            '\t' => out.push_str("    "),
            _ if winansi(c).is_some() => out.push(c),
            _ => {
                out.push('·');
                count += 1;
            }
        }
    }
    (out, count)
}

/// Word wrapping preserves explicit newlines and breaks oversized tokens.
/// A zero width is treated as one, ensuring progress for every input.
pub fn wrap(text: &str, max_chars: usize) -> Vec<String> {
    let width = max_chars.max(1);
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let mut chars = word.chars().peekable();
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            while chars.peek().is_some() {
                if line.chars().count() == width {
                    out.push(std::mem::take(&mut line));
                }
                line.push(chars.next().unwrap());
            }
        }
        out.push(line);
    }
    out
}

fn literal(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        let b = winansi(c).unwrap_or(b' ');
        match b {
            b'\\' | b'(' | b')' => {
                out.push('\\');
                out.push(b as char);
            }
            128..=255 => out.push_str(&format!("\\{b:03o}")),
            _ => out.push(b as char),
        }
    }
    out
}

struct Line {
    text: String,
    size: f32,
    bold: bool,
    notes: bool,
}
impl Line {
    fn height(&self) -> f32 {
        if self.size > 14.0 {
            24.0
        } else if self.bold {
            20.0
        } else {
            14.0
        }
    }
}
fn add(lines: &mut Vec<Line>, text: &str, size: f32, bold: bool, notes: bool) {
    let (clean, _) = sanitize_winansi(text);
    let width = if size == 18.0 {
        45
    } else if bold {
        68
    } else {
        92
    };
    for text in wrap(&clean, width) {
        lines.push(Line {
            text,
            size,
            bold,
            notes,
        });
    }
}
fn heading(lines: &mut Vec<Line>, text: &str) {
    add(lines, "", 10.5, false, false);
    add(lines, text, 13.0, true, false);
}

fn text_op(text: &str, size: f32, bold: bool, x: f32, y: f32) -> String {
    // Bound worst-case wide glyphs without cutting text. Ordinary prose retains
    // its natural spacing; the 92-character cap alone cannot bound Helvetica.
    let available = 558.0 - x;
    let width: f32 = text
        .chars()
        .map(|c| match c {
            'i' | 'l' | 'I' | '.' | ',' | ':' | ';' | '!' | '\'' | ' ' => 0.28,
            'm' | 'w' | 'M' | 'W' | '@' => 1.0,
            _ => 0.67,
        })
        .sum::<f32>()
        * size;
    let scale = (available / width.max(1.0) * 100.0).min(100.0);
    format!(
        "BT /F{} {size} Tf {scale:.2} Tz 1 0 0 1 {x} {y} Tm ({}) Tj ET\n",
        if bold { 2 } else { 1 },
        literal(text)
    )
}

/// Render uncompressed content streams with measured byte offsets in xref.
pub fn render(doc: &MeetingDoc) -> Vec<u8> {
    let mut lines = Vec::new();
    add(&mut lines, &doc.title, 18.0, true, false);
    add(
        &mut lines,
        &format!("{} · Duration: {}", doc.date_line, doc.duration),
        10.5,
        false,
        false,
    );
    let mut summary = Vec::new();
    let mut actions = Vec::new();
    let mut in_actions = false;
    if let Some(md) = &doc.summary_md {
        for line in md.lines() {
            let key = line
                .trim()
                .trim_start_matches('#')
                .trim()
                .trim_matches('*')
                .trim()
                .trim_end_matches(':')
                .to_ascii_lowercase();
            if key == "summary" {
                in_actions = false;
                continue;
            }
            if key == "action items" {
                in_actions = true;
                continue;
            }
            if in_actions {
                actions.push(line);
            } else {
                summary.push(line);
            }
        }
    }
    let summary_text = summary.join("\n");
    let actions_text = actions.join("\n");
    heading(&mut lines, "Summary");
    add(
        &mut lines,
        if summary.iter().all(|s| s.trim().is_empty()) {
            "No summary available."
        } else {
            &summary_text
        },
        10.5,
        false,
        false,
    );
    if let Some(note) = &doc.coverage_note {
        add(&mut lines, note, 10.5, false, false);
    }
    heading(&mut lines, "Action items");
    add(
        &mut lines,
        if actions.iter().all(|s| s.trim().is_empty()) {
            "No action items recorded."
        } else {
            &actions_text
        },
        10.5,
        false,
        false,
    );
    if let Some(notes) = &doc.notes {
        if !notes.trim().is_empty() {
            heading(
                &mut lines,
                "Your notes (verbatim — typed by you, not model-generated)",
            );
            // Preserve typed spaces and line breaks in this verbatim section.
            // Break at character boundaries only; do not interpret Markdown.
            let (clean, _) = sanitize_winansi(notes);
            for paragraph in clean.split('\n') {
                let chars: Vec<_> = paragraph.trim_end_matches('\r').chars().collect();
                if chars.is_empty() {
                    add(&mut lines, "", 10.5, false, true);
                }
                for chunk in chars.chunks(92) {
                    lines.push(Line {
                        text: chunk.iter().collect(),
                        size: 10.5,
                        bold: false,
                        notes: true,
                    });
                }
            }
        }
    }
    heading(&mut lines, "Transcript (appendix)");
    for line in &doc.transcript_lines {
        add(&mut lines, line, 10.5, false, false);
    }

    let filename = doc
        .source_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let replacements: usize = [
        &doc.title,
        &doc.date_line,
        &doc.duration,
        doc.summary_md.as_deref().unwrap_or(""),
        doc.notes.as_deref().unwrap_or(""),
        doc.coverage_note.as_deref().unwrap_or(""),
        &filename,
    ]
    .into_iter()
    .chain(doc.transcript_lines.iter().map(String::as_str))
    .map(|s| sanitize_winansi(s).1)
    .sum();
    let mut pages: Vec<Vec<&Line>> = vec![Vec::new()];
    let mut used = 0.0;
    for (i, line) in lines.iter().enumerate() {
        let keep_next = if line.bold {
            lines.get(i + 1).map_or(0.0, Line::height)
        } else {
            0.0
        };
        if used + line.height() + keep_next > 644.0 && !pages.last().unwrap().is_empty() {
            pages.push(Vec::new());
            used = 0.0;
        }
        if used == 0.0 && line.text.is_empty() {
            continue;
        }
        used += line.height();
        pages.last_mut().unwrap().push(line);
    }
    let count = pages.len();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!(
            "<< /Type /Pages /Count {count} /Kids [{}] >>",
            (0..count)
                .map(|i| format!("{} 0 R", 5 + i * 2))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".into(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>"
            .into(),
    ];
    for (i, page) in pages.iter().enumerate() {
        let mut stream = String::new();
        let mut y = 738.0;
        for line in page {
            if line.notes {
                stream.push_str(&format!("q 0.5 w 46 {} m 46 {} l S Q\n", y - 3.0, y + 11.0));
            }
            stream.push_str(&text_op(
                &line.text,
                line.size,
                line.bold,
                if line.notes { 60.0 } else { 54.0 },
                y,
            ));
            y -= line.height();
        }
        let footer = sanitize_winansi(&format!(
            "{} — page {} of {count} · full transcript: {filename}",
            doc.title,
            i + 1
        ))
        .0
        .replace(['\n', '\r'], " ");
        stream.push_str(&text_op(&footer, 8.0, false, 54.0, 60.0));
        if replacements > 0 {
            stream.push_str(&text_op(&format!("{replacements} characters outside this PDF font's repertoire were replaced; the .md transcript has the exact text."), 8.0, false, 54.0, 46.0));
        }
        objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {} 0 R >>", 6 + i * 2));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{}endstream",
            stream.len(),
            stream
        ));
    }
    let mut pdf = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = vec![0];
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()).as_bytes());
    for offset in offsets.iter().skip(1) {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len()
        )
        .as_bytes(),
    );
    pdf
}

/// Safe display-oriented stem (Unicode is retained in filenames).
pub fn pdf_file_stem(title: &str, when: &chrono::DateTime<chrono::Local>) -> String {
    let title: String = title
        .chars()
        .take(60)
        .map(|c| {
            if matches!(c, '/' | ':' | '\\') || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!(
        "Meeting - {} - {}",
        title.trim(),
        when.format("%Y-%m-%d %H%M")
    )
}

/// Reserve a destination with create_new so concurrent exports never overwrite
/// each other, then fsync a sibling temporary file and rename it into place.
/// A failed export removes only its own reservation and temporary file.
pub fn write_atomic(bytes: &[u8], dir: &Path, base_name: &str) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        !base_name.is_empty()
            && !base_name.contains(['/', '\\'])
            && !base_name.chars().any(char::is_control)
            && base_name != "."
            && base_name != "..",
        "invalid PDF file stem"
    );
    fs::create_dir_all(dir)?;
    for suffix in 1..=20 {
        let stem = if suffix == 1 {
            base_name.to_string()
        } else {
            format!("{base_name} -{suffix}")
        };
        let path = dir.join(format!("{stem}.pdf"));
        let reservation = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        };
        drop(reservation);
        let tmp = dir.join(format!(".{stem}.pdf.tmp"));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => f,
            Err(e) => {
                let _ = fs::remove_file(&path);
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(e.into());
            }
        };
        let result = (|| -> std::io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, &path)
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            let _ = fs::remove_file(&path);
            return Err(e.into());
        }
        return Ok(path);
    }
    anyhow::bail!("all 20 PDF filenames are already occupied")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn doc(n: usize) -> MeetingDoc {
        MeetingDoc { title: "Product planning".into(), date_line: "September 9, 2026".into(), duration: "45 minutes".into(), summary_md: Some("## Summary\nThe team agreed to ship the local meeting companion.\n\n## Action items\n- Alex: validate export before Friday.\n- Sam: review crash recovery.".into()), notes: Some("Keep the customer’s exact words: “offline first.”\n  Preserve this indent.\nDecision → review the prototype…\n日本語".into()), transcript_lines: (0..n).map(|i| format!("[00:{:02}:00] {}: Review capture reliability and the local export experience.", i % 60, if i % 2 == 0 { "Me" } else { "Them" })).collect(), coverage_note: Some("Summary covers the complete meeting.".into()), source_path: PathBuf::from("2026-09-09-product-planning.md") }
    }
    fn tempdir() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "vzt-pdf-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    #[test]
    fn renders_a_valid_pdf_header_and_trailer() {
        let bytes = render(&doc(1));
        let text = String::from_utf8_lossy(&bytes);
        assert!(bytes.starts_with(b"%PDF-1.4\n"));
        assert!(text.ends_with("%%EOF\n"));
        assert!(text.contains("startxref\n"));
        assert!(text.contains("/WinAnsiEncoding"));
    }
    #[test]
    fn xref_offsets_point_at_their_objects() {
        let bytes = render(&doc(100));
        let text = String::from_utf8_lossy(&bytes);
        let start: usize = text
            .rsplit("startxref\n")
            .next()
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let xref = std::str::from_utf8(&bytes[start..]).unwrap();
        assert!(xref.starts_with("xref\n"));
        let count: usize = xref
            .lines()
            .nth(1)
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        for (i, row) in xref.lines().skip(3).take(count - 1).enumerate() {
            let offset: usize = row[..10].parse().unwrap();
            assert!(bytes[offset..].starts_with(format!("{} 0 obj\n", i + 1).as_bytes()));
        }
    }
    #[test]
    fn page_count_matches_the_pages_object() {
        let bytes = render(&doc(400));
        let text = String::from_utf8_lossy(&bytes);
        let count: usize = text
            .split("/Count ")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(count > 1);
        assert_eq!(count, text.matches("/Type /Page ").count());
    }
    #[test]
    fn wrap_breaks_on_words_and_hard_breaks_a_long_token() {
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        let token = "x".repeat(200);
        let lines = wrap(&token, 92);
        assert!(lines.iter().all(|s| s.chars().count() <= 92));
        assert_eq!(lines.concat(), token);
        assert_eq!(wrap("ééé", 2), vec!["éé", "é"]);
        assert_eq!(wrap("ab", 0), vec!["a", "b"]);
    }
    #[test]
    fn sanitize_maps_smart_punctuation_and_counts_replacements() {
        assert_eq!(sanitize_winansi("“”‘’–—…•"), ("“”‘’–—…•".into(), 0));
        assert_eq!(literal("“—…•"), "\\223\\227\\205\\225");
        assert_eq!(sanitize_winansi("日本語→a\u{200b}"), ("···->a".into(), 3));
        assert_eq!(sanitize_winansi("…").1, 0);
    }
    #[test]
    fn write_atomic_never_clobbers_and_suffixes_on_collision() {
        let dir = tempdir();
        let first = write_atomic(b"first", &dir, "Meeting").unwrap();
        let second = write_atomic(b"second", &dir, "Meeting").unwrap();
        assert_eq!(second.file_name().unwrap(), "Meeting -2.pdf");
        assert_eq!(fs::read(first).unwrap(), b"first");
        for _ in 3..=20 {
            write_atomic(b"next", &dir, "Meeting").unwrap();
        }
        assert!(write_atomic(b"overflow", &dir, "Meeting").is_err());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 20);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn write_atomic_leaves_no_tmp_file_behind() {
        let dir = tempdir();
        write_atomic(&render(&doc(1)), &dir, "Meeting").unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        assert!(dir.join("Meeting.pdf").exists());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn notes_section_is_labelled_verbatim() {
        let bytes = render(&doc(1));
        let text = String::from_utf8_lossy(&bytes);
        // Parentheses must be escaped in a PDF literal string.
        assert!(text.contains("Your notes \\(verbatim"));
        assert!(text.contains("typed by you, not model-generated"));
        assert!(text.contains("0.5 w 46"));
        assert!(text.contains("3 characters outside"));
    }
    #[test]
    #[ignore = "writes a manual CoreGraphics preview fixture"]
    fn writes_a_sample_pdf() {
        let bytes = render(&doc(105));
        assert!(String::from_utf8_lossy(&bytes).contains("/Count 3 "));
        fs::write("/tmp/vzt-pdf-smoke.pdf", bytes).unwrap();
        println!("Wrote /tmp/vzt-pdf-smoke.pdf (3 pages)");
    }
}
