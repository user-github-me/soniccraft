import math, struct, sys
def ref(sr, channels, path):
    n = sr // 2
    click = sr // 4
    frames = []
    for i in range(n):
        s = 0.1 * math.sin(2 * math.pi * 440 * i / sr)
        if i == click:
            s += 0.8
        frames.append(s)
    data = bytearray()
    for s in frames:
        for c in range(channels):
            v = s if c == 0 else 0.5 * s
            q = max(-32768, min(32767, int(round(v * 32768))))
            data += struct.pack('<h', q)
    hdr = b'RIFF' + struct.pack('<I', 36 + len(data)) + b'WAVE' + b'fmt ' + struct.pack('<IHHIIHH', 16, 1, channels, sr, sr * 2 * channels, 2 * channels, 16) + b'data' + struct.pack('<I', len(data))
    open(path, 'wb').write(hdr + data)
ref(48000, 1, 'ref_mono48k.wav')
ref(44100, 2, 'ref_stereo44k.wav')
