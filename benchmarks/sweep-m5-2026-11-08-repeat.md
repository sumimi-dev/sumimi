# The sweeps — 2026-11-08 (the second round) — macOS, Apple M5, 10 cores

**Nine sweeps, back to back, on the same box** ⇒ ⇒ ⇒ so they are comparable ✓ ⇒
⇒ ⇒ (the tool: `sumimi-gotest sweep` ✓ ⇒ the second implementation ✓ ⇒ 64
connections ✓ ⇒ 20 s a point ✓ ⇒ **a fresh server per point** ✓ ⇒ ⇒ ⇒ ⇒ and
the peak is the kernel's `ru_maxrss` ✓ ⇒ ⇒ ⇒ ⇒ which macOS **resets on
exec** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ so it is the server's own ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **but it counts
the shared pages** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ and it wobbles ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **which is why the deployment table is a Linux table** ✓ ✓).

**Three runs per variant** ⇒ ⇒ ⇒ ⇒ so the range is visible ✓ ⇒ ⇒ ⇒ ⇒ ⇒ the
ranges are in the brackets ✓ ✓.

1. **v0.4.1** ⇒ the release before the memory work ✓
2. **v0.4.2** ⇒ the release with the seven fixes ✓
3. **v0.4.2 + `MALLOC_MMAP_THRESHOLD_=65536`** ⇒ ⇒ ⇒ **and it does nothing
   here** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ **because macOS does not use glibc's allocator** ✓ ⇒
   ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **the variable is a glibc knob ✓ ✓**

⇒ ⇒ **the same shape as the Linux box** ✓ ✓ ⇒ ⇒ ⇒ ⇒ the peak follows the
⇒ ⇒ ⇒ ⇒ ⇒ rate ✓ ⇒ ⇒ ⇒ ⇒ ⇒ at the low rates v0.4.1 and v0.4.2 are the
⇒ ⇒ ⇒ ⇒ ⇒ same ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ and at the top v0.4.2 is cheaper ✓ ✓

⇒ ⇒ **but the numbers are not reproducible across rounds** ✓ ⇒ ⇒ ⇒ ⇒ the
⇒ ⇒ ⇒ ⇒ ⇒ same binary at 5 000/s measured **104 MB** in the first round of
⇒ ⇒ ⇒ ⇒ ⇒ the day ✓ ⇒ ⇒ ⇒ ⇒ ⇒ and **133 MB** in this one ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **Linux repeats to ±2 MB ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ so the table only believes Linux ✓ ✓**

⇒ ⇒ [#18](https://github.com/sumimi-dev/sumimi/issues/18) ✓ ⇒ ⇒ ⇒ ⇒ the whole
⇒ ⇒ ⇒ ⇒ ⇒ record ✓ ⇒ ⇒ ⇒ ⇒ ⇒ the discussion there ✓ ✓.

---


# The measurement — 2026-11-08 15:15 UTC

**The machine that was measured**: Apple M5 · 10 cores · 32.0 GB · macos
**The machine that measured it**: Apple M5 · 10 cores · 32.0 GB · darwin
**The server**: Sumimi 0.4.1

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 000 | 100 000 | 0 | 5 000/s | 133 MB |
| 10 000 | 200 000 | 199 999 | 0 | 10 000/s | 143 MB |
| 20 000 | 400 000 | 399 999 | 0 | 20 000/s | 151 MB |
| 40 000 | 799 998 | 799 996 | 0 | 40 000/s | 315 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 0 | 1 MB | 1 ms |
| 10 000 | 0 | 2 MB | 2 ms |
| 20 000 | 0 | 3 MB | 1 ms |
| 40 000 | 0 | 4 MB | 287 ms |

The peak is the kernel's `ru_maxrss` — the server cannot report its own high-water mark on this platform.

# The measurement — 2026-11-08 15:16 UTC

**The machine that was measured**: Apple M5 · 10 cores · 32.0 GB · macos
**The machine that measured it**: Apple M5 · 10 cores · 32.0 GB · darwin
**The server**: Sumimi 0.4.1

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 000 | 100 000 | 0 | 5 000/s | 132 MB |
| 10 000 | 200 000 | 200 000 | 0 | 10 000/s | 141 MB |
| 20 000 | 400 000 | 399 999 | 0 | 20 000/s | 200 MB |
| 40 000 | 799 999 | 799 997 | 0 | 40 000/s | 317 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 0 | 1 MB | 4 ms |
| 10 000 | 0 | 2 MB | 1 ms |
| 20 000 | 0 | 2 MB | 1 ms |
| 40 000 | 0 | 4 MB | 24 ms |

The peak is the kernel's `ru_maxrss` — the server cannot report its own high-water mark on this platform.

# The measurement — 2026-11-08 15:26 UTC

**The machine that was measured**: Apple M5 · 10 cores · 32.0 GB · macos
**The machine that measured it**: Apple M5 · 10 cores · 32.0 GB · darwin
**The server**: Sumimi 0.4.1

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 000 | 100 000 | 0 | 5 000/s | 134 MB |
| 10 000 | 199 999 | 199 999 | 0 | 10 000/s | 144 MB |
| 20 000 | 400 000 | 399 999 | 0 | 20 000/s | 151 MB |
| 40 000 | 799 997 | 799 994 | 0 | 40 000/s | 322 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 0 | 1 MB | 3 ms |
| 10 000 | 0 | 2 MB | 1 ms |
| 20 000 | 0 | 3 MB | 2 ms |
| 40 000 | 0 | 4 MB | 25 ms |

The peak is the kernel's `ru_maxrss` — the server cannot report its own high-water mark on this platform.

# The measurement — 2026-11-08 15:18 UTC

**The machine that was measured**: Apple M5 · 10 cores · 32.0 GB · macos
**The machine that measured it**: Apple M5 · 10 cores · 32.0 GB · darwin
**The server**: Sumimi 0.4.2

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 000 | 100 000 | 0 | 5 000/s | 138 MB |
| 10 000 | 200 000 | 199 999 | 0 | 10 000/s | 147 MB |
| 20 000 | 400 000 | 399 998 | 0 | 20 000/s | 247 MB |
| 40 000 | 799 997 | 799 996 | 0 | 40 000/s | 270 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 0 | 1 MB | 74 ms |
| 10 000 | 0 | 2 MB | 72 ms |
| 20 000 | 3 000 | 2 MB | 267 ms |
| 40 000 | 997 | 4 MB | 313 ms |

The peak is the kernel's `ru_maxrss` — the server cannot report its own high-water mark on this platform.

# The measurement — 2026-11-08 15:20 UTC

**The machine that was measured**: Apple M5 · 10 cores · 32.0 GB · macos
**The machine that measured it**: Apple M5 · 10 cores · 32.0 GB · darwin
**The server**: Sumimi 0.4.2

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 000 | 100 000 | 0 | 5 000/s | 138 MB |
| 10 000 | 200 000 | 199 999 | 0 | 10 000/s | 147 MB |
| 20 000 | 399 999 | 399 998 | 0 | 20 000/s | 246 MB |
| 40 000 | 799 998 | 799 997 | 0 | 40 000/s | 277 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 0 | 1 MB | 72 ms |
| 10 000 | 0 | 2 MB | 71 ms |
| 20 000 | 2 999 | 2 MB | 270 ms |
| 40 000 | 998 | 4 MB | 285 ms |

The peak is the kernel's `ru_maxrss` — the server cannot report its own high-water mark on this platform.

# The measurement — 2026-11-08 15:27 UTC

**The machine that was measured**: Apple M5 · 10 cores · 32.0 GB · macos
**The machine that measured it**: Apple M5 · 10 cores · 32.0 GB · darwin
**The server**: Sumimi 0.4.2

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 000 | 100 000 | 0 | 5 000/s | 139 MB |
| 10 000 | 200 000 | 199 999 | 0 | 10 000/s | 146 MB |
| 20 000 | 400 000 | 399 999 | 0 | 20 000/s | 227 MB |
| 40 000 | 799 999 | 799 997 | 0 | 40 000/s | 269 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 0 | 1 MB | 72 ms |
| 10 000 | 0 | 2 MB | 71 ms |
| 20 000 | 3 000 | 2 MB | 267 ms |
| 40 000 | 999 | 4 MB | 287 ms |

The peak is the kernel's `ru_maxrss` — the server cannot report its own high-water mark on this platform.

# The measurement — 2026-11-08 15:21 UTC

**The machine that was measured**: Apple M5 · 10 cores · 32.0 GB · macos
**The machine that measured it**: Apple M5 · 10 cores · 32.0 GB · darwin
**The server**: Sumimi 0.4.2

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 000 | 100 000 | 0 | 5 000/s | 139 MB |
| 10 000 | 200 000 | 199 999 | 0 | 10 000/s | 146 MB |
| 20 000 | 400 000 | 399 999 | 0 | 20 000/s | 246 MB |
| 40 000 | 799 998 | 799 996 | 0 | 40 000/s | 284 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 0 | 1 MB | 72 ms |
| 10 000 | 0 | 2 MB | 69 ms |
| 20 000 | 3 000 | 2 MB | 276 ms |
| 40 000 | 998 | 4 MB | 305 ms |

The peak is the kernel's `ru_maxrss` — the server cannot report its own high-water mark on this platform.

# The measurement — 2026-11-08 15:23 UTC

**The machine that was measured**: Apple M5 · 10 cores · 32.0 GB · macos
**The machine that measured it**: Apple M5 · 10 cores · 32.0 GB · darwin
**The server**: Sumimi 0.4.2

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 000 | 100 000 | 0 | 5 000/s | 137 MB |
| 10 000 | 200 000 | 200 000 | 0 | 10 000/s | 145 MB |
| 20 000 | 400 000 | 399 999 | 0 | 20 000/s | 226 MB |
| 40 000 | 799 998 | 799 997 | 0 | 40 000/s | 268 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 0 | 1 MB | 72 ms |
| 10 000 | 0 | 2 MB | 70 ms |
| 20 000 | 3 000 | 2 MB | 275 ms |
| 40 000 | 998 | 4 MB | 283 ms |

The peak is the kernel's `ru_maxrss` — the server cannot report its own high-water mark on this platform.

# The measurement — 2026-11-08 15:29 UTC

**The machine that was measured**: Apple M5 · 10 cores · 32.0 GB · macos
**The machine that measured it**: Apple M5 · 10 cores · 32.0 GB · darwin
**The server**: Sumimi 0.4.2

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 000 | 100 000 | 0 | 5 000/s | 139 MB |
| 10 000 | 200 000 | 199 999 | 0 | 10 000/s | 146 MB |
| 20 000 | 399 999 | 399 999 | 0 | 20 000/s | 244 MB |
| 40 000 | 799 998 | 799 997 | 0 | 40 000/s | 267 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 0 | 1 MB | 72 ms |
| 10 000 | 0 | 2 MB | 71 ms |
| 20 000 | 2 999 | 2 MB | 268 ms |
| 40 000 | 998 | 4 MB | 287 ms |

The peak is the kernel's `ru_maxrss` — the server cannot report its own high-water mark on this platform.
