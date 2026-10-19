#!/usr/bin/env bash
# Generate SonicCraft's royalty-free example assets with ffmpeg (lavfi only, no samples, no downloads).
#
# Every sound is synthesized from closed-form expressions (aevalsrc / aeval) and seeded noise, so the
# output is a pure function of this script and the ffmpeg build: no wall clock, no unseeded randomness,
# and -bitexact muxing (no encoder/version tags inside the WAV files). The committed files were
# produced with ffmpeg 9.0.2; other ffmpeg versions may differ in the last bit of a few samples.
#
#   audio/synthwave_loop.wav  8 bars of 2/4 at 120 BPM (Am-F-C-G, two bars per chord): a pumping
#                             detuned pad over an eighth-note octave bass. Exactly 8.000 s =
#                             384000 frames, 48 kHz stereo 16-bit. Seamless: the file is one period of
#                             a strictly periodic signal, taken after the effects reached steady state,
#                             and the script proves it (period 2 == period 3 of a 3-period render).
#   sfx/whoosh.wav            1.2 s stereo: pink noise through a swept resonant band-pass (280 Hz to
#                             ~5.5 kHz) with a left-to-right pan; swells to a peak at 0.85 s.
#   sfx/impact.wav            2.0 s mono: pitch-dropping sub boom + noise transient + low rumble.
#   sfx/chime.wav             1.5 s mono: two-note bell chime (E5 then A5).
#
# Usage:
#   scripts/generate-example-assets.sh            regenerate and install into examples/*/assets/
#   scripts/generate-example-assets.sh --out DIR  generate into DIR/{audio,sfx}/ only (no install)
#   scripts/generate-example-assets.sh --verify   check the committed assets with ffprobe (no ffmpeg
#                                                 synthesis; works with any ffmpeg version)
#
# Environment: FFMPEG / FFPROBE override the binaries (default: ffmpeg / ffprobe on PATH).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FFMPEG="${FFMPEG:-ffmpeg}"
FFPROBE="${FFPROBE:-ffprobe}"
SR=48000
MAX_TOTAL_BYTES=$((6 * 1000 * 1000)) # keep the committed example assets below 6 MB in total

# Which example uses which asset (paths relative to examples/<name>/assets/).
EXAMPLE_ASSETS=(
  "tech-briefing:audio/synthwave_loop.wav sfx/whoosh.wav"
  "two-person-podcast:audio/synthwave_loop.wav sfx/chime.wav"
  "offline-demo:audio/synthwave_loop.wav sfx/whoosh.wav sfx/impact.wav sfx/chime.wav"
)

die() {
  echo "error: $*" >&2
  exit 1
}

log() {
  echo "==> $*" >&2
}

need() {
  command -v "$1" >/dev/null 2>&1 || die "'$1' not found on PATH (set ${2:-} to override)"
}

# Temporary directory base without a trailing slash (macOS sets TMPDIR with one).
tmp_base() {
  local base="${TMPDIR:-/tmp}"
  printf '%s' "${base%/}"
}

# Deterministic WAV output: no metadata, no encoder tag, 16-bit PCM.
ff() {
  "$FFMPEG" -hide_banner -nostdin -loglevel error -y "$@"
}
WAV_OUT=(-map_metadata -1 -fflags +bitexact -flags:a +bitexact -c:a pcm_s16le)

# ---------------------------------------------------------------------------------------------------
# synthwave_loop.wav
# ---------------------------------------------------------------------------------------------------
# The expression is periodic in t with period 8 s by construction: every component is a function of
# mod(t, 8) (or of a divisor of 8 s), and every oscillator's phase is measured from its own note onset.
#
# Pad: chord k (k = 0..3) starts at 2k seconds. Its local time is tau = mod(t + 8 - 2k, 8). The layer
# fades in over 0.6 s (raised cosine), sustains until tau = 2 and fades out over the next 0.6 s while
# the following chord fades in (complementary raised cosines), so the crossfade at the 8 s seam is the
# same as every other chord change. Each note is two detuned voices (different per channel for width)
# with three harmonics. A beat-synced "pump" (alpha-function dip, continuous at every beat) adds motion.
#
# Bass: 32 eighth notes per loop (0.25 s each); the root of the current chord, an octave up on the
# 3rd and 7th eighth of each chord. Each note restarts its phase and its envelope is zero at both
# ends of its slot, so note boundaries (and the seam) are continuous.

# Chord tones (Hz): Am (A3 C4 E4), F (A3 C4 F4), C (G3 C4 E4), G (G3 B3 D4).
CHORDS=(
  "220.000 261.626 329.628"
  "220.000 261.626 349.228"
  "195.998 261.626 329.628"
  "195.998 246.942 293.665"
)
# Detune factors per channel: left -8/+6 cents, right -6/+8 cents.
DETUNE_L="0.995390 1.003472"
DETUNE_R="0.996540 1.004632"

# One pad voice: harmonics 1, 2, 3 of frequency $1 (local time in var 0, scratch var 9).
pad_voice() {
  printf '(st(9,2*PI*%s*ld(0));sin(ld(9))+0.3*sin(2*ld(9))+0.12*sin(3*ld(9)))' "$1"
}

# Raised-cosine chord envelope of local time tau (var 0): attack 0.6 s, sustain to 2 s, release 0.6 s.
PAD_ENV='if(lt(ld(0),0.6),0.5-0.5*cos(PI*ld(0)/0.6),if(lt(ld(0),2),1,0.5+0.5*cos(PI*(ld(0)-2)/0.6)))'

# Pad for one channel: accumulates the active chord layers into var 2.
pad_channel() {
  local detunes="$1" expr="st(2,0)" k notes f d voices offset
  for k in 0 1 2 3; do
    notes="${CHORDS[$k]}"
    voices=""
    for f in $notes; do
      for d in $detunes; do
        voices="${voices:+$voices+}$(pad_voice "$(awk -v f="$f" -v d="$d" 'BEGIN { printf "%.6f", f * d }')")"
      done
    done
    offset=$(((8 - 2 * k) % 8))
    expr="$expr;st(0,mod(t+$offset,8));if(lt(ld(0),2.6),st(2,ld(2)+$PAD_ENV*($voices)))"
  done
  printf '%s' "$expr"
}

# Pump (var 4): 1 - 0.3 * alpha(x), x = time since the last quarter note (0.5 s at 120 BPM).
PUMP='st(3,mod(t,0.5));st(4,1-0.3*(ld(3)/0.04)*exp(1-ld(3)/0.04))'

# Bass (var 8): eighth-note index in var 5, local time in var 6, frequency in var 7.
BASS='st(5,floor(mod(t,8)/0.25));st(6,mod(t,0.25));'
BASS+='st(7,if(eq(floor(ld(5)/8),0),110,if(eq(floor(ld(5)/8),1),87.307,if(eq(floor(ld(5)/8),2),130.813,97.999)))*if(eq(mod(ld(5),4),2),2,1));'
BASS+='st(8,tanh(1.6*(sin(2*PI*ld(7)*ld(6))+0.5*exp(-ld(6)/0.06)*sin(4*PI*ld(7)*ld(6))+0.25*exp(-ld(6)/0.04)*sin(6*PI*ld(7)*ld(6))))'
BASS+='*min(1,ld(6)/0.004)*(0.55+0.45*exp(-ld(6)/0.09))*min(1,(0.25-ld(6))/0.025))'

LOOP_PAD_GAIN=0.045
LOOP_BASS_GAIN=0.30
LOOP_MASTER_GAIN=1.45
LOOP_PERIOD_FRAMES=$((8 * SR))

gen_loop() {
  local out="$1" tmp="$2/loop-3periods.wav"
  local left right
  left="$(pad_channel "$DETUNE_L");$PUMP;$BASS;$LOOP_PAD_GAIN*ld(2)*ld(4)+$LOOP_BASS_GAIN*ld(8)"
  right="$(pad_channel "$DETUNE_R");$PUMP;$BASS;$LOOP_PAD_GAIN*ld(2)*ld(4)+$LOOP_BASS_GAIN*ld(8)"
  # Render three periods through the effects, then keep the middle one: by then the echo and filter
  # state are in steady state, so the kept period continues seamlessly into itself.
  ff -f lavfi -i "aevalsrc='$left|$right':s=$SR:d=24:c=stereo" \
    -af "lowpass=f=6500,aecho=0.9:0.7:125|250:0.25|0.12,volume=$LOOP_MASTER_GAIN" \
    "${WAV_OUT[@]}" "$tmp"
  ff -i "$tmp" -af "atrim=start_sample=$LOOP_PERIOD_FRAMES:end_sample=$((2 * LOOP_PERIOD_FRAMES))" \
    "${WAV_OUT[@]}" "$out"
  verify_seam "$tmp" "$2"
}

# Proves the loop is seamless: in the 3-period render, period 2 (the shipped file) and period 3 must
# be sample-identical up to 1 LSB. If they are, playing the file twice in a row is exactly the
# continuous signal, i.e. the loop point is inaudible and sample-accurate.
verify_seam() {
  local tmp="$1" work="$2" max
  max="$("$FFMPEG" -hide_banner -nostdin -i "$tmp" -i "$tmp" -filter_complex \
    "[0:a]atrim=start_sample=$LOOP_PERIOD_FRAMES:end_sample=$((2 * LOOP_PERIOD_FRAMES)),asetpts=N/SR/TB[a];\
[1:a]atrim=start_sample=$((2 * LOOP_PERIOD_FRAMES)):end_sample=$((3 * LOOP_PERIOD_FRAMES)),asetpts=N/SR/TB[b];\
[a][b]amerge=inputs=2,pan=stereo|c0=c0-c2|c1=c1-c3,volumedetect" -f null - 2>&1 |
    sed -n 's/.*max_volume: \(.*\) dB/\1/p')"
  [ -n "$max" ] || die "could not measure the loop seam"
  awk -v m="$max" 'BEGIN { exit !(m == "-inf" || m + 0 <= -84.0) }' ||
    die "loop is not seamless: period 2 and period 3 differ by up to $max dBFS (more than 1 LSB)"
  log "loop seam verified: period 2 vs period 3 max difference ${max} dBFS"
}

# ---------------------------------------------------------------------------------------------------
# whoosh.wav: seeded pink noise -> Chamberlin state-variable band-pass whose centre frequency follows
# the swell envelope (vars: 0 envelope, 1 coefficient, 2 low, 3 band, 4 high) -> left-to-right pan.
# ---------------------------------------------------------------------------------------------------
WHOOSH_SECONDS=1.2
WHOOSH_GAIN=1.0

gen_whoosh() {
  local out="$1"
  local env='st(0,if(lt(t,0.85),pow(sin(PI/2*t/0.85),2),pow(cos(PI/2*min(1,(t-0.85)/0.35)),2)))'
  local svf="$env;st(1,2*sin(PI*(280+5200*pow(ld(0),1.4))/$SR));st(2,ld(2)+ld(1)*ld(3));st(4,val(0)-ld(2)-0.45*ld(3));st(3,ld(1)*ld(4)+ld(3));$WHOOSH_GAIN*ld(3)*ld(0)"
  local theta="(0.18+0.64*t/$WHOOSH_SECONDS)*PI/2"
  ff -f lavfi -i "anoisesrc=d=$WHOOSH_SECONDS:c=pink:r=$SR:a=1:seed=20261019" \
    -af "aeval='$svf':c=mono,aeval='val(0)*cos($theta)|val(0)*sin($theta)':c=stereo" \
    "${WAV_OUT[@]}" "$out"
}

# ---------------------------------------------------------------------------------------------------
# impact.wav: sub boom with an exponential pitch drop (closed-form phase), a short low-passed noise
# transient and a darker rumble tail (one-pole low-pass states in vars 1-3), soft-clipped. A 0.8 ms
# onset ramp and a raised-cosine fade over the last 80 ms make the file start and end at exactly zero.
# ---------------------------------------------------------------------------------------------------
IMPACT_SECONDS=2.0
IMPACT_GAIN=0.50

gen_impact() {
  local out="$1"
  local boom='(1-exp(-t/0.002))*exp(-t/0.45)*sin(2*PI*(38*t+120*0.07*(1-exp(-t/0.07))))'
  local click='st(1,ld(1)+0.2791*(val(0)-ld(1)));0.6*exp(-t/0.025)*ld(1)'
  local rumble='st(2,ld(2)+0.05101*(val(0)-ld(2)));st(3,ld(3)+0.05101*(ld(2)-ld(3)));0.9*exp(-t/0.35)*ld(3)'
  local fade="if(gt(t,$IMPACT_SECONDS-0.08),0.5+0.5*cos(PI*(t-($IMPACT_SECONDS-0.08))/0.08),1)"
  local expr="st(5,min(1,t/0.0008)*($boom+($click)+($rumble)));$IMPACT_GAIN*tanh(1.4*ld(5))/tanh(1.4)*$fade"
  ff -f lavfi -i "anoisesrc=d=$IMPACT_SECONDS:c=white:r=$SR:a=1:seed=4242" \
    -af "aeval='$expr':c=mono" "${WAV_OUT[@]}" "$out"
}

# ---------------------------------------------------------------------------------------------------
# chime.wav: two bell notes (E5 at 0 s, A5 at 0.14 s), each four partials with their own decays and a
# 1.5 ms attack; raised-cosine fade over the last 250 ms.
# ---------------------------------------------------------------------------------------------------
CHIME_SECONDS=1.5
CHIME_GAIN=0.47

chime_note() { # frequency onset
  local f="$1" on="$2" x="(t-$2)"
  printf 'if(gte(t,%s),(1-exp(-%s/0.0015))*(exp(-%s/0.45)*sin(2*PI*%s*%s)+0.35*exp(-%s/0.30)*sin(4*PI*%s*%s)+0.12*exp(-%s/0.18)*sin(6*PI*%s*%s)+0.06*exp(-%s/0.09)*sin(8.4*PI*%s*%s)),0)' \
    "$on" "$x" "$x" "$f" "$x" "$x" "$f" "$x" "$x" "$f" "$x" "$x" "$f" "$x"
}

gen_chime() {
  local out="$1"
  local fade="if(gt(t,$CHIME_SECONDS-0.25),0.5+0.5*cos(PI*(t-($CHIME_SECONDS-0.25))/0.25),1)"
  local expr="$CHIME_GAIN*(0.55*$(chime_note 659.255 0)+0.45*$(chime_note 880.000 0.14))*$fade"
  ff -f lavfi -i "aevalsrc='$expr':s=$SR:d=$CHIME_SECONDS:c=mono" "${WAV_OUT[@]}" "$out"
}

# ---------------------------------------------------------------------------------------------------
# Verification
# ---------------------------------------------------------------------------------------------------

# check_wav FILE CHANNELS FRAMES: codec, rate, channels, exact length, and a sane peak level.
check_wav() {
  local file="$1" channels="$2" frames="$3" info peak
  [ -f "$file" ] || die "missing asset: $file"
  info="$("$FFPROBE" -v error -select_streams a:0 -count_packets \
    -show_entries stream=codec_name,sample_rate,channels,bits_per_sample,duration_ts \
    -of default=noprint_wrappers=1 "$file")"
  grep -qx 'codec_name=pcm_s16le' <<<"$info" || die "$file: expected pcm_s16le"$'\n'"$info"
  grep -qx "sample_rate=$SR" <<<"$info" || die "$file: expected $SR Hz"$'\n'"$info"
  grep -qx "channels=$channels" <<<"$info" || die "$file: expected $channels channel(s)"$'\n'"$info"
  grep -qx 'bits_per_sample=16' <<<"$info" || die "$file: expected 16-bit"$'\n'"$info"
  grep -qx "duration_ts=$frames" <<<"$info" || die "$file: expected $frames frames"$'\n'"$info"
  peak="$("$FFMPEG" -hide_banner -nostdin -i "$file" -af volumedetect -f null - 2>&1 |
    sed -n 's/.*max_volume: \(.*\) dB/\1/p')"
  awk -v p="$peak" 'BEGIN { exit !(p + 0 <= -0.5 && p + 0 >= -12) }' ||
    die "$file: peak level $peak dBFS outside [-12, -0.5]"
  local shown="${file#"$ROOT"/}"
  [ "$shown" != "$file" ] || shown="$(basename "$(dirname "$file")")/$(basename "$file")"
  printf '    ok  %-50s %s ch  %7d frames  peak %6s dBFS  %8d bytes\n' \
    "$shown" "$channels" "$frames" "$peak" "$(wc -c <"$file" | tr -d ' ')" >&2
}

check_asset() { # FILE (dispatches on the file name)
  case "$(basename "$1")" in
  synthwave_loop.wav) check_wav "$1" 2 $((8 * SR)) ;;
  whoosh.wav) check_wav "$1" 2 $((SR * 12 / 10)) ;;
  impact.wav) check_wav "$1" 1 $((SR * 2)) ;;
  chime.wav) check_wav "$1" 1 $((SR * 3 / 2)) ;;
  *) die "unknown asset $1" ;;
  esac
}

verify_installed() {
  local entry name files f total=0
  for entry in "${EXAMPLE_ASSETS[@]}"; do
    name="${entry%%:*}"
    files="${entry#*:}"
    for f in $files; do
      check_asset "$ROOT/examples/$name/assets/$f"
      total=$((total + $(wc -c <"$ROOT/examples/$name/assets/$f" | tr -d ' ')))
    done
  done
  [ "$total" -lt "$MAX_TOTAL_BYTES" ] || die "example assets total $total bytes (limit $MAX_TOTAL_BYTES)"
  log "all example assets verified ($total bytes in total)"
}

generate_into() { # DIR
  local dir="$1" work
  mkdir -p "$dir/audio" "$dir/sfx"
  work="$(mktemp -d "$(tmp_base)/soniccraft-assets.XXXXXX")"
  # shellcheck disable=SC2064 # expand now: the trap must remove this run's directory
  trap "rm -rf '$work'" EXIT
  log "synthesizing audio/synthwave_loop.wav (3 periods, keeping the steady-state middle one)"
  gen_loop "$dir/audio/synthwave_loop.wav" "$work"
  log "synthesizing sfx/whoosh.wav"
  gen_whoosh "$dir/sfx/whoosh.wav"
  log "synthesizing sfx/impact.wav"
  gen_impact "$dir/sfx/impact.wav"
  log "synthesizing sfx/chime.wav"
  gen_chime "$dir/sfx/chime.wav"
  local f
  for f in audio/synthwave_loop.wav sfx/whoosh.wav sfx/impact.wav sfx/chime.wav; do
    check_asset "$dir/$f"
  done
}

install_examples() { # BUILD_DIR
  local build="$1" entry name files f
  for entry in "${EXAMPLE_ASSETS[@]}"; do
    name="${entry%%:*}"
    files="${entry#*:}"
    [ -d "$ROOT/examples/$name" ] || die "examples/$name does not exist"
    for f in $files; do
      mkdir -p "$(dirname "$ROOT/examples/$name/assets/$f")"
      cp "$build/$f" "$ROOT/examples/$name/assets/$f"
    done
    log "installed examples/$name/assets/"
  done
}

main() {
  need "$FFPROBE" FFPROBE
  need "$FFMPEG" FFMPEG
  case "${1:-}" in
  --verify)
    verify_installed
    ;;
  --out)
    [ -n "${2:-}" ] || die "--out needs a directory"
    generate_into "$2"
    ;;
  "")
    local build
    build="$(mktemp -d "$(tmp_base)/soniccraft-assets-build.XXXXXX")"
    generate_into "$build"
    install_examples "$build"
    rm -rf "$build"
    verify_installed
    ;;
  -h | --help)
    awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "${BASH_SOURCE[0]}"
    ;;
  *)
    die "unknown argument '$1' (try --help)"
    ;;
  esac
}

main "$@"
