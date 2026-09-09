#!/usr/bin/env bash
# Reproducible local TTS fixtures. Absolute WER includes TTS artefacts; compare
# relative WER between pipelines, not these voices against natural speech.
set -euo pipefail

# Optional second argument revises only the echo pair using its existing PCM,
# avoiding voice-engine variation in the six frozen comparison fixtures.
accuracy_mode=${2:-all}
case "$accuracy_mode" in
  all|--echo-only) ;;
  *) printf 'Usage: %s [output-dir] [--echo-only]\n' "$0" >&2; exit 1 ;;
esac

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
echo.them.voice=Samantha
echo.me.interjection.voice=Daniel
quiet.filter=volume=0.08
echo.me.filter=adelay=350|350,volume=0.25
echo.pause_seconds=1.5
echo.revision=2 (1.5s digital silence before genuine interjection)
echo.files=echo.them.wav,echo.me.wav
echo.ref=Them sentence followed by genuine Daniel interjection on its own line; speaker bleed excluded
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
if [[ "$accuracy_mode" == all ]]; then
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

else
  # Preserve the other six cases and their metadata byte-for-byte. An existing
  # pair is required: this mode never silently synthesizes replacement speech.
  for accuracy_existing in names.wav names.ref.txt echo.me.wav echo.ref.txt corpus.meta; do
    [[ -f "$accuracy_out/$accuracy_existing" ]] || { printf 'Echo-only mode needs %s\n' "$accuracy_existing" >&2; exit 1; }
  done
  cp "$accuracy_out/names.wav" "$accuracy_tmp/names.wav"
  cp "$accuracy_out/names.ref.txt" "$accuracy_tmp/names.ref.txt"
  cp "$accuracy_out/corpus.meta" "$accuracy_tmp/corpus.meta"
fi

cp "$accuracy_tmp/names.wav" "$accuracy_tmp/them.wav"
cp "$accuracy_tmp/names.ref.txt" "$accuracy_tmp/them.ref.txt"
if [[ "$accuracy_mode" == all ]]; then
speak me Daniel 150 'yes that works for me'
ff -i "$accuracy_tmp/them.wav" -i "$accuracy_tmp/me.wav" \
  -f lavfi -i 'anullsrc=r=16000:cl=mono:d=1.5' \
  -filter_complex '[0:a]adelay=350|350,volume=0.25[echo];[echo][2:a][1:a]concat=n=3:v=0:a=1[out]' \
  -map '[out]' -ar 16000 -ac 1 -c:a pcm_s16le "$accuracy_tmp/me-mixed.wav"
else
  # Insert the pause directly into the existing PCM: the delayed/gain-reduced
  # echo and Daniel audio remain bit-identical, even if say varies across runs.
  python3 - "$accuracy_out" "$accuracy_tmp" <<'PY_ECHO'
from pathlib import Path
import sys, wave
root, tmp = map(Path, sys.argv[1:])
meta = dict(line.split('=', 1) for line in (root/'corpus.meta').read_text().splitlines() if '=' in line)
with wave.open(str(root/'names.wav')) as wav:
    assert (wav.getframerate(), wav.getnchannels(), wav.getsampwidth()) == (16000, 1, 2)
    split = wav.getnframes() + 5600  # 350ms delay at 16kHz
with wave.open(str(root/'echo.me.wav')) as wav:
    assert (wav.getframerate(), wav.getnchannels(), wav.getsampwidth()) == (16000, 1, 2)
    params = wav.getparams()
    pcm = wav.readframes(wav.getnframes())
old_gap = round(float(meta.get('echo.pause_seconds', '0')) * 16000)
assert len(pcm) > (split + old_gap) * 2, 'Missing genuine interjection'
assert not any(pcm[split*2:(split+old_gap)*2]), 'Recorded pause is not digital silence'
with wave.open(str(tmp/'me-mixed.wav'), 'wb') as wav:
    wav.setparams(params)
    wav.writeframes(pcm[:split*2] + bytes(24000*2) + pcm[(split+old_gap)*2:])
ref = (root/'echo.ref.txt').read_text().splitlines()
assert ref[1:] == ['yes that works for me'], 'Unexpected echo reference'
(tmp/'me.ref.txt').write_text('yes that works for me\n')
lines = [line for line in (tmp/'corpus.meta').read_text().splitlines() if not line.startswith(('echo.pause_seconds=', 'echo.revision='))]
lines += ['echo.pause_seconds=1.5', 'echo.revision=2 (1.5s digital silence before genuine interjection)']
(tmp/'corpus.meta').write_text('\n'.join(lines)+'\n')
PY_ECHO
fi
mv "$accuracy_tmp/me-mixed.wav" "$accuracy_tmp/me.wav"

# One reference for the merged dual-source case: retain the Them sentence and
# the genuine interjection exactly once. The mic copy is speaker bleed.
cat "$accuracy_tmp/them.ref.txt" "$accuracy_tmp/me.ref.txt" > "$accuracy_tmp/echo.ref.txt"
mv "$accuracy_tmp/them.wav" "$accuracy_tmp/echo.them.wav"
mv "$accuracy_tmp/me.wav" "$accuracy_tmp/echo.me.wav"

# Migrate the previous generator layout without leaving phantom single-source
# cases. Keep the old generated fixtures in a subdirectory outside discovery.
for accuracy_legacy in them.wav me.wav them.ref.txt me.ref.txt; do
  if [[ -e "$accuracy_out/$accuracy_legacy" ]]; then
    mkdir -p "$accuracy_out/legacy-standalone-echo"
    mv "$accuracy_out/$accuracy_legacy" "$accuracy_out/legacy-standalone-echo/$accuracy_legacy"
  fi
done

# Seven cases, eight WAVs: echo is a pair discovered from echo.ref.txt.
# AIFFs and filter intermediates stay in the temporary build directory.
accuracy_cases=(echo)
accuracy_audios=(echo.them echo.me)
if [[ "$accuracy_mode" == all ]]; then
  accuracy_cases=(names numbers code quiet seam blips echo)
  accuracy_audios=(names numbers code quiet seam blips echo.them echo.me)
fi
for accuracy_case in "${accuracy_cases[@]}"; do
  mv "$accuracy_tmp/$accuracy_case.ref.txt" "$accuracy_out/$accuracy_case.ref.txt"
done
for accuracy_audio in "${accuracy_audios[@]}"; do
  mv "$accuracy_tmp/$accuracy_audio.wav" "$accuracy_out/$accuracy_audio.wav"
  accuracy_duration=$(duration "$accuracy_out/$accuracy_audio.wav")
  accuracy_peak=$(ffmpeg -hide_banner -nostdin -i "$accuracy_out/$accuracy_audio.wav" -af volumedetect -f null - 2>&1 | awk '/max_volume:/ { print $(NF-1), $NF }')
  printf '%s.wav duration=%ss peak=%s\n' "$accuracy_audio" "$accuracy_duration" "$accuracy_peak"
done
mv "$accuracy_tmp/corpus.meta" "$accuracy_out/corpus.meta"
