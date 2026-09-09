#!/usr/bin/env bash
# Reproducible local TTS fixtures. Absolute WER includes TTS artefacts; compare
# relative WER between pipelines, not these voices against natural speech.
set -euo pipefail

for accuracy_tool in say ffmpeg ffprobe python3; do
  command -v "$accuracy_tool" >/dev/null 2>&1 || {
    printf 'Required tool missing: %s\n' "$accuracy_tool" >&2
    exit 1
  }
done

# Resolve symlinks before creating anything: corpus output must never live in
# this checkout, its owning repository, or Documents (including via symlinks).
accuracy_script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
accuracy_checkout=$(cd -- "$accuracy_script_dir/.." && pwd -P)
accuracy_out=$(python3 - "${1:-/tmp/vzt-accuracy}" "$accuracy_checkout" <<'PY'
from pathlib import Path
import subprocess, sys
out = Path(sys.argv[1]).expanduser().resolve()
checkout = Path(sys.argv[2]).resolve()
forbidden = [checkout, (Path.home() / 'Documents').resolve()]
try:
    common = subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', '--git-common-dir'], text=True).strip()
    forbidden.append((checkout / common).resolve().parent)
except (OSError, subprocess.CalledProcessError):
    pass
if any(out == root or root in out.parents for root in forbidden):
    raise SystemExit('Refusing corpus output inside the repository or ~/Documents: ' + str(out))
print(out)
PY
)
mkdir -p -- "$accuracy_out"
accuracy_tmp=$(mktemp -d "$accuracy_out/.build.XXXXXXXX")
trap 'rm -rf -- "$accuracy_tmp"' EXIT

accuracy_voices=$(say -v '?')
has_voice() { awk -v voice="$1" '$1 == voice { found=1 } END { exit !found }' <<< "$accuracy_voices"; }
for accuracy_voice in Samantha Daniel; do
  has_voice "$accuracy_voice" || { printf 'Required voice missing: %s\n' "$accuracy_voice" >&2; exit 1; }
done
accuracy_irish=Moira
if ! has_voice Moira; then
  accuracy_irish=Karen
  has_voice Karen || { printf 'Neither Moira nor fallback Karen is installed\n' >&2; exit 1; }
fi
printf 'voices: Samantha, Daniel, %s\n' "$accuracy_irish"
cat > "$accuracy_tmp/corpus.meta" <<META
format=16000 Hz mono PCM signed 16-bit
voices=Samantha,Daniel,$accuracy_irish
names.voice=Samantha
numbers.voice=Daniel
code.voice=$accuracy_irish
quiet.voice=Samantha
seam.voice=Samantha
blips.voice=$accuracy_irish
them.voice=Samantha
me.interjection.voice=Daniel
quiet.filter=volume=0.08
me.echo.filter=adelay=350|350,volume=0.25
me.ref=only the genuine interjection; delayed speaker bleed is not new speech
comparison=absolute WER includes TTS artefacts; only relative pipeline WER is claimed
META

ff() { ffmpeg -hide_banner -loglevel error -nostdin -y "$@"; }
duration() { ffprobe -v error -show_entries format=duration -of csv=p=0 "$1"; }
# Reference text is exactly the input argument handed to say, not an ASR output.
speak() {
  local case_name=$1 voice=$2 rate=$3 words=$4
  printf '%s\n' "$words" > "$accuracy_tmp/$case_name.ref.txt"
  say -v "$voice" -r "$rate" -o "$accuracy_tmp/$case_name.aiff" "$words"
  ff -i "$accuracy_tmp/$case_name.aiff" -ar 16000 -ac 1 -c:a pcm_s16le "$accuracy_tmp/$case_name.wav"
}

accuracy_names="Vonzelle Brown met Aoife O'Sullivan and Rajesh Krishnamurthy on Tuesday."
accuracy_numbers="The invoice is one thousand four hundred and eighty two dollars and thirty seven cents, due on March third at four fifteen p.m."
accuracy_code="The function is called parse config file and it lives in model manager dot r s."
speak names Samantha 175 "$accuracy_names"
speak numbers Daniel 175 "$accuracy_numbers"
speak code "$accuracy_irish" 175 "$accuracy_code"
ff -i "$accuracy_tmp/names.wav" -af volume=0.08 -ar 16000 -ac 1 -c:a pcm_s16le "$accuracy_tmp/quiet.wav"
cp "$accuracy_tmp/names.ref.txt" "$accuracy_tmp/quiet.ref.txt"

# One continuous, punctuation-free utterance avoids long sentence pauses. Keep
# the actual words intact; tempo adjustment makes a reproducible 45-second take.
accuracy_seam="today we are reviewing the meeting companion and walking through the entire recording workflow from the moment a call begins until the final notes are saved on the desktop we need to preserve each speaker and every important decision while the conversation continues across the boundaries between audio chunks the first topic is reliable capture and the second topic is accurate names numbers and technical terms so please remember that the release owner is Priya and the review takes place next Friday at four fifteen in the afternoon after that review the team will compare the original transcript with the summary and confirm that early decisions are still present then we will check the notes window and verify that a short pause does not erase the last word of a sentence because every detail matters when a long meeting is divided into smaller pieces for local transcription and the final report must preserve the complete conversation"
speak seam Samantha 205 "$accuracy_seam"
accuracy_tempo=$(python3 - "$(duration "$accuracy_tmp/seam.wav")" <<'PY'
import sys
ratio = float(sys.argv[1]) / 45
assert 0.5 <= ratio <= 2, 'seam tempo would exceed the supported range'
print(ratio)
PY
)
ff -i "$accuracy_tmp/seam.wav" -af "atempo=$accuracy_tempo" -ar 16000 -ac 1 -c:a pcm_s16le "$accuracy_tmp/seam-timed.wav"
mv "$accuracy_tmp/seam-timed.wav" "$accuracy_tmp/seam.wav"
python3 - "$(duration "$accuracy_tmp/seam.wav")" <<'PY'
import sys
seconds = float(sys.argv[1])
if seconds <= 35:
    raise SystemExit(f'Seam clip is only {seconds:.3f}s; must exceed 35s')
PY

accuracy_blips="We reviewed the release plan and agreed on the next steps. The team will check the notes and send an update tomorrow."
speak blips "$accuracy_irish" 175 "$accuracy_blips"
accuracy_tempo=$(python3 - "$(duration "$accuracy_tmp/blips.wav")" <<'PY'
import sys
ratio = float(sys.argv[1]) / 8
assert 0.5 <= ratio <= 2, 'blips tempo would exceed the supported range'
print(ratio)
PY
)
ff -i "$accuracy_tmp/blips.wav" -af "atempo=$accuracy_tempo,apad,atrim=duration=8" -ar 16000 -ac 1 -c:a pcm_s16le "$accuracy_tmp/blips-speech.wav"
# Insert a gap at a quiet point near the sentence boundary. Sample-index trims
# preserve every speech sample; the noise bursts occur only in inserted gaps.
accuracy_cut=$(python3 - "$accuracy_tmp/blips-speech.wav" <<'PY'
import array, sys, wave
with wave.open(sys.argv[1]) as w:
    assert w.getnchannels() == 1 and w.getframerate() == 16000 and w.getsampwidth() == 2
    samples = array.array('h', w.readframes(w.getnframes()))
if sys.byteorder != 'little': samples.byteswap()
width = 1600
choices = range(40000, 88000, 160)
start = min(choices, key=lambda i: sum(v*v for v in samples[i:i+width]))
print(start + width // 2)
PY
)
accuracy_noise_delay=$(python3 - "$accuracy_cut" <<'PY'
import sys
print(round(int(sys.argv[1]) / 16 + 200))
PY
)
ff -i "$accuracy_tmp/blips-speech.wav" \
  -f lavfi -i 'anullsrc=r=16000:cl=mono:d=0.8' \
  -f lavfi -i 'anoisesrc=r=16000:d=0.2:a=0.12:seed=42' \
  -f lavfi -i 'anoisesrc=r=16000:d=0.2:a=0.12:seed=43' \
  -filter_complex "[0:a]asplit=2[a][b];[a]atrim=end_sample=$accuracy_cut,asetpts=PTS-STARTPTS[first];[b]atrim=start_sample=$accuracy_cut,asetpts=PTS-STARTPTS[last];[1:a]asplit=2[gap][tail];[first][gap][last][tail]concat=n=4:v=0:a=1[speech];[2:a]adelay=$accuracy_noise_delay[n1];[3:a]adelay=9000[n2];[speech][n1][n2]amix=inputs=3:duration=first:normalize=0[out]" \
  -map '[out]' -ar 16000 -ac 1 -c:a pcm_s16le "$accuracy_tmp/blips.wav"
printf 'blips.speech_seconds=8\nblips.inserted_gap_sample=%s\nblips.noise_delays_ms=%s,9000\nblips.noise_duration_seconds=0.2\n' "$accuracy_cut" "$accuracy_noise_delay" >> "$accuracy_tmp/corpus.meta"

cp "$accuracy_tmp/names.wav" "$accuracy_tmp/them.wav"
cp "$accuracy_tmp/names.ref.txt" "$accuracy_tmp/them.ref.txt"
speak me Daniel 150 'yes that works for me'
ff -i "$accuracy_tmp/them.wav" -i "$accuracy_tmp/me.wav" \
  -filter_complex '[0:a]adelay=350|350,volume=0.25[echo];[echo][1:a]concat=n=2:v=0:a=1[out]' \
  -map '[out]' -ar 16000 -ac 1 -c:a pcm_s16le "$accuracy_tmp/me-mixed.wav"
mv "$accuracy_tmp/me-mixed.wav" "$accuracy_tmp/me.wav"

# Publish only the eight final wav/ref pairs (seven cases: echo has two sources).
# AIFFs and filter intermediates stay in the temporary build directory.
for accuracy_case in names numbers code quiet seam blips them me; do
  mv "$accuracy_tmp/$accuracy_case.wav" "$accuracy_out/$accuracy_case.wav"
  mv "$accuracy_tmp/$accuracy_case.ref.txt" "$accuracy_out/$accuracy_case.ref.txt"
  accuracy_duration=$(duration "$accuracy_out/$accuracy_case.wav")
  accuracy_peak=$(ffmpeg -hide_banner -nostdin -i "$accuracy_out/$accuracy_case.wav" -af volumedetect -f null - 2>&1 | awk '/max_volume:/ { print $(NF-1), $NF }')
  printf '%s.wav duration=%ss peak=%s\n' "$accuracy_case" "$accuracy_duration" "$accuracy_peak"
done
mv "$accuracy_tmp/corpus.meta" "$accuracy_out/corpus.meta"
