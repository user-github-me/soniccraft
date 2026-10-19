#!/usr/bin/env python3
"""Print cargo-deny's hash of license files, for `[[licenses.clarify]]` entries in deny.toml.

cargo-deny hashes a license file with XXH32 (seed 0) after replacing each line's trailing CR/LF with a
single LF; a clarification applies only while the hashes match. Usage:

  python3 scripts/deny-license-hash.py ~/.cargo/registry/src/*/mp3lame-sys-0.1.11/LICENSE
"""

import sys

MASK = 0xFFFFFFFF
P1, P2, P3, P4, P5 = 0x9E3779B1, 0x85EBCA77, 0xC2B2AE3D, 0x27D4EB2F, 0x165667B1


def rotl(x, r):
    return ((x << r) | (x >> (32 - r))) & MASK


def xxh32(data, seed=0):
    n, i = len(data), 0

    def word(j):
        return int.from_bytes(data[j:j + 4], "little")

    if n >= 16:
        v = [(seed + P1 + P2) & MASK, (seed + P2) & MASK, seed & MASK, (seed - P1) & MASK]
        while i <= n - 16:
            for k in range(4):
                v[k] = (rotl((v[k] + word(i) * P2) & MASK, 13) * P1) & MASK
                i += 4
        h = (rotl(v[0], 1) + rotl(v[1], 7) + rotl(v[2], 12) + rotl(v[3], 18)) & MASK
    else:
        h = (seed + P5) & MASK
    h = (h + n) & MASK
    while i + 4 <= n:
        h = (rotl((h + word(i) * P3) & MASK, 17) * P4) & MASK
        i += 4
    while i < n:
        h = (rotl((h + data[i] * P5) & MASK, 11) * P1) & MASK
        i += 1
    h ^= h >> 15
    h = (h * P2) & MASK
    h ^= h >> 13
    h = (h * P3) & MASK
    h ^= h >> 16
    return h


def normalized(data):
    lines = data.split(b"\n")
    if lines and lines[-1] == b"":
        lines.pop()
    return b"".join(line.rstrip(b"\r\n") + b"\n" for line in lines)


assert xxh32(b"") == 0x02CC5D05 and xxh32(b"abc") == 0x32D153FF  # reference vectors

if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__.strip())
    for path in sys.argv[1:]:
        with open(path, "rb") as file:
            print(f"0x{xxh32(normalized(file.read())):08x}  {path}")
