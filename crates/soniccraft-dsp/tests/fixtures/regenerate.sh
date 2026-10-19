#!/bin/bash
# Regenerates the encoded `tone_click_*` fixtures (needs python3 and an ffmpeg with libmp3lame,
# libopus and libvorbis). Run from this directory.
set -euo pipefail
python3 -I gen_ref.py
G=(-hide_banner -loglevel error -y)
O=(-map_metadata -1 -fflags +bitexact -flags:a +bitexact)
ffmpeg "${G[@]}" -i ref_mono48k.wav "${O[@]}" -c:a libmp3lame -b:a 96k tone_click_mono48k.mp3
ffmpeg "${G[@]}" -i ref_mono48k.wav "${O[@]}" -c:a flac tone_click_mono48k.flac
ffmpeg "${G[@]}" -i ref_mono48k.wav "${O[@]}" -c:a pcm_s16be -f aiff tone_click_mono48k.aiff
ffmpeg "${G[@]}" -i ref_mono48k.wav "${O[@]}" -c:a libopus -b:a 64k tone_click_mono48k.opus
ffmpeg "${G[@]}" -i ref_stereo44k.wav "${O[@]}" -c:a libvorbis -q:a 3 tone_click_stereo44k.ogg
ffmpeg "${G[@]}" -i ref_stereo44k.wav "${O[@]}" -c:a aac -b:a 128k tone_click_stereo44k.m4a
# VBR MP3 without a Xing/Info header: 0.3 s of loud noise (large frames) then 1.7 s of a quiet sine
# (small frames), so a length estimated from the first frames is far too short.
ffmpeg "${G[@]}" -f lavfi -i "anoisesrc=d=0.3:c=white:r=44100:a=0.5:seed=7" -f lavfi -i "sine=f=220:r=44100:d=1.7" \
  -filter_complex "[0][1]concat=n=2:v=0:a=1,volume=0.5" "${O[@]}" -ac 1 -c:a libmp3lame -q:a 4 -write_xing 0 \
  vbr_noxing_mono44k.mp3
rm ref_mono48k.wav ref_stereo44k.wav
