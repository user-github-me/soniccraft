# soniccraft-dsp test fixtures

Small, self-contained audio files used by `tests/decode_fixtures.rs`.

## Encoded formats (`tone_click_*`)

Made by `regenerate.sh` from the reference signal in `gen_ref.py`: 0.5 s of a 440 Hz sine at
amplitude 0.1 with a single click (+0.8) at exactly 0.25 s, quantized to 16-bit.

| file | format | layout |
|---|---|---|
| `tone_click_mono48k.mp3` | MP3 (libmp3lame 96k CBR, LAME/Info tag) | mono, 48 kHz |
| `tone_click_mono48k.flac` | FLAC | mono, 48 kHz |
| `tone_click_mono48k.aiff` | AIFF (s16be) | mono, 48 kHz |
| `tone_click_mono48k.opus` | Ogg Opus (libopus 64k) — decodable only through the ffmpeg fallback | mono, 48 kHz |
| `tone_click_stereo44k.ogg` | Ogg Vorbis (libvorbis q3) | stereo (R = 0.5 × L), 44.1 kHz |
| `tone_click_stereo44k.m4a` | M4A / AAC-LC 128k with an `elst` edit list (1024 priming frames) | stereo (R = 0.5 × L), 44.1 kHz |
| `vbr_noxing_mono44k.mp3` | VBR MP3 (libmp3lame `-q:a 4 -write_xing 0`) **without** a Xing/Info header: 0.3 s of noise, then 1.7 s of a sine. 78 MPEG frames, so 89 856 decoded frames (as ffmpeg decodes it); a length estimated from the first frames' size is about 1.9 s | mono, 44.1 kHz |

## Real-world WAV quirks

| file | origin | quirk |
|---|---|---|
| `say_fllr.wav` | macOS `say -o … --data-format=LEI16@16000 "Hi there"` | `JUNK` chunk before `fmt `, 4008-byte `FLLR` chunk, audio at byte 4096 (12014 frames, 16 kHz mono) |
| `piped_list_ffff.wav` | ffmpeg writing WAV to a pipe, truncated to 6000 frames | `LIST` chunk, RIFF and `data` sizes `0xFFFFFFFF` (24 kHz mono) |
| `data_ffff.wav` | streaming WAV header, truncated to 6000 frames | `data` size `0xFFFFFFFF` (24 kHz mono) |
| `data_zero.wav` | same audio as `data_ffff.wav` | RIFF and `data` sizes `0` |
