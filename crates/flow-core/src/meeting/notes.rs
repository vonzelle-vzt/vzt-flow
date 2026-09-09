//! Live meeting notepad: a crash-safe `.notes.txt` sidecar that is merged
//! into the transcript's `## My notes` section on finalize.
//!
//! Deliberately `.notes.txt`, not `.notes.md` — `meeting::list_meetings` and
//! the MCP `meeting_transcript` tool both select on the `.md` extension, so a
//! markdown sidecar would show up as a phantom meeting in both.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

/// Heading the merged notes section is written under in the transcript.
pub const NOTES_HEADING: &str = "## My notes";
pub const NOTES_START: &str = "<!-- vzt-flow:notes:start -->";
pub const NOTES_END: &str = "<!-- vzt-flow:notes:end -->";

/// Disclaimer placed under [`NOTES_HEADING`] so a reader knows this section
/// is typed, not model-generated.
pub const NOTES_DISCLAIMER: &str = "> _Typed by you during the meeting; not model-generated._";

/// The `.notes.txt` sidecar path for a given transcript path.
///
/// Deliberately `.notes.txt`, not `.notes.md` — see module docs.
pub fn notes_path_for(transcript: &Path) -> PathBuf {
    let stem = transcript
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let file_name = format!("{stem}.notes.txt");
    match transcript.parent() {
        Some(dir) => dir.join(file_name),
        None => PathBuf::from(file_name),
    }
}

/// Reads the notes sidecar, or `""` when absent/unreadable.
pub fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// Writes using an exclusively created, unique sibling temp file. The caller
/// orders revisions; concurrent callers publish in rename-completion order.
pub fn save_atomic(path: &Path, text: &str) -> Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let (tmp, mut file) = loop {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let tmp = parent.join(format!(
            ".{stem}.notes.{}.{nanos}.{seq}.tmp",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => break (tmp, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("failed to create {}", tmp.display())),
        }
    };
    let result = (|| -> std::io::Result<()> {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("failed to atomically save {}", path.display()))
}

// Byte boundaries of the first complete marker pair. Only entire lines match;
// the next end marker after the first start marker closes the region.
fn notes_region(markdown: &str) -> Option<(usize, usize)> {
    let mut offset = 0;
    let mut start = None;
    for line in markdown.split_inclusive('\n') {
        let exact = line.strip_suffix('\n').unwrap_or(line);
        let exact = exact.strip_suffix('\r').unwrap_or(exact);
        if start.is_none() && exact == NOTES_START {
            start = Some(offset);
        } else if let Some(begin) = start {
            if exact == NOTES_END {
                return Some((begin, offset + line.len()));
            }
        }
        offset += line.len();
    }
    None
}

/// Idempotent atomic upsert. Read errors leave the original untouched. Call
/// only after the capture writer has closed; this does not lock other writers.
pub fn merge_into_transcript(transcript: &Path, notes: &str) -> Result<bool> {
    let existing = fs::read_to_string(transcript)
        .with_context(|| format!("failed to read transcript {}", transcript.display()))?;
    if notes.trim().is_empty() && notes_region(&existing).is_none() {
        return Ok(false);
    }
    save_atomic(transcript, &replace_notes_section(&existing, notes))?;
    Ok(true)
}

/// Replace the first complete marker-delimited section, or append a new one.
/// Headings in the transcript or typed notes are never used as delimiters.
pub fn replace_notes_section(markdown: &str, notes: &str) -> String {
    let section = format!("{NOTES_START}\n\n{NOTES_DISCLAIMER}\n\n{notes}\n{NOTES_END}\n");
    if let Some((start, end)) = notes_region(markdown) {
        format!("{}{section}{}", &markdown[..start], &markdown[end..])
    } else {
        let separator = if markdown.is_empty() || markdown.ends_with("\n\n") {
            ""
        } else if markdown.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        format!("{markdown}{separator}{NOTES_HEADING}\n{section}")
    }
}

/// Prefixes each non-empty line of `notes` with `Me (note): `.
pub fn as_summary_lines(notes: &str) -> Vec<String> {
    notes
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| format!("Me (note): {line}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("flow-core-notes-test-{}-{}", std::process::id(), n));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn notes_path_is_a_txt_sidecar_not_md() {
        let dir = unique_temp_dir();
        let transcript = dir.join("x.md");
        let notes = notes_path_for(&transcript);
        assert_eq!(notes.file_name().unwrap().to_str().unwrap(), "x.notes.txt");
        assert_eq!(notes.extension().unwrap(), "txt");

        // Phantom-meeting guard: both files exist, list_meetings should only
        // ever see the real transcript.
        fs::write(&transcript, "# Meeting\n\nsome content\n").unwrap();
        fs::write(&notes, "scratch notes\n").unwrap();

        let meetings = super::super::list_meetings(&dir, 10).unwrap();
        assert_eq!(meetings.len(), 1);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_atomic_round_trips() {
        let dir = unique_temp_dir();
        let path = dir.join("y.notes.txt");

        save_atomic(&path, "hello world").unwrap();

        assert_eq!(read(&path), "hello world");
        let tmp_path = path.with_extension("txt.tmp");
        assert!(!tmp_path.exists());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn merge_appends_the_heading_and_disclaimer_before_the_summary() {
        let dir = unique_temp_dir();
        let transcript = dir.join("z.md");
        fs::write(&transcript, "# Meeting\n\nTranscript body\n").unwrap();

        let did_merge = merge_into_transcript(&transcript, "remember to follow up").unwrap();
        assert!(did_merge);

        let content = fs::read_to_string(&transcript).unwrap();
        assert!(content.contains(NOTES_HEADING));
        assert!(content.contains(NOTES_DISCLAIMER));
        assert!(content.contains("remember to follow up"));
        // Heading should come after the existing body.
        let body_pos = content.find("Transcript body").unwrap();
        let heading_pos = content.find(NOTES_HEADING).unwrap();
        assert!(heading_pos > body_pos);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn merge_is_a_noop_for_whitespace_only_notes() {
        let dir = unique_temp_dir();
        let transcript = dir.join("w.md");
        fs::write(&transcript, "# Meeting\n\nbody\n").unwrap();

        let did_merge = merge_into_transcript(&transcript, "   \n\t  \n").unwrap();
        assert!(!did_merge);

        let content = fs::read_to_string(&transcript).unwrap();
        assert!(!content.contains(NOTES_HEADING));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replace_notes_section_preserves_the_summary_that_follows() {
        let markdown = format!(
            "# Meeting\n\nbody\n\n{NOTES_HEADING}\n{NOTES_START}\n\n{NOTES_DISCLAIMER}\n\nold notes\n{NOTES_END}\n\n## Summary\n\nthe summary text\n"
        );

        let updated = replace_notes_section(&markdown, "new notes");

        assert!(updated.contains("new notes"));
        assert!(!updated.contains("old notes"));
        assert!(updated.contains("## Summary"));
        assert!(updated.contains("the summary text"));
        // Summary section should still follow the notes section.
        let notes_pos = updated.find(NOTES_HEADING).unwrap();
        let summary_pos = updated.find("## Summary").unwrap();
        assert!(summary_pos > notes_pos);
    }

    #[test]
    fn as_summary_lines_prefixes_every_line() {
        let notes = "first point\n\nsecond point\n   \nthird point";
        let lines = as_summary_lines(notes);
        assert_eq!(
            lines,
            vec![
                "Me (note): first point".to_string(),
                "Me (note): second point".to_string(),
                "Me (note): third point".to_string(),
            ]
        );
    }
    #[test]
    fn replace_ignores_a_heading_lookalike_inside_transcript_text() {
        let body = "# Meeting\n[00:01:00] Me: I said ## My notes in conversation\n[00:02:00] Them: important decision\n";
        let initial = format!(
            "{}\n## Summary\nKeep this summary\n",
            replace_notes_section(body, "old")
        );
        let updated = replace_notes_section(&initial, "new");
        assert!(updated.starts_with(body));
        assert!(updated.ends_with("## Summary\nKeep this summary\n"));
        assert!(!updated.contains("\nold\n"));
        // Marker substrings also remain ordinary transcript text.
        let lookalike = format!("Speaker quoted {NOTES_START} inline\nkeep me\n");
        assert!(replace_notes_section(&lookalike, "notes").starts_with(&lookalike));
    }

    #[test]
    fn replace_keeps_a_user_typed_heading_inside_notes() {
        let notes = "first\n## my own heading\nlast";
        let once = replace_notes_section("# Meeting\n", notes);
        let twice = replace_notes_section(&once, notes);
        assert_eq!(once, twice);
        assert!(twice.contains(notes));
    }

    #[test]
    fn merge_is_idempotent() {
        let dir = unique_temp_dir();
        let path = dir.join("meeting.md");
        fs::write(&path, "# Meeting\nbody\n").unwrap();
        merge_into_transcript(&path, "typed notes").unwrap();
        let once = fs::read_to_string(&path).unwrap();
        merge_into_transcript(&path, "typed notes").unwrap();
        assert_eq!(once, fs::read_to_string(&path).unwrap());
        assert_eq!(once.matches(NOTES_START).count(), 1);
        assert!(merge_into_transcript(&path, "   ").unwrap());
        assert!(!fs::read_to_string(&path).unwrap().contains("typed notes"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn merge_propagates_a_read_error() {
        let dir = unique_temp_dir();
        assert!(merge_into_transcript(&dir, "notes").is_err());
        assert!(dir.is_dir());
        assert!(merge_into_transcript(&dir.join("missing.md"), "notes").is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_atomic_temp_name_is_unique() {
        let dir = unique_temp_dir();
        let path = dir.join("meeting.notes.txt");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|i| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    save_atomic(&path, &format!("revision {i}")).unwrap();
                })
            })
            .collect();
        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }
        // Caller-ordered latest revision is published after both racing saves.
        save_atomic(&path, "latest revision").unwrap();
        assert_eq!(read(&path), "latest revision");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
