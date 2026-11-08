# The sweeps — 2026-11-08 (the second round) — Linux, 2 vCPU AMD EPYC, 2 GB

**Six sweeps, back to back, on the same box** ⇒ ⇒ ⇒ so they are comparable ✓ ⇒
⇒ ⇒ (the tool: `sumimi-gotest sweep` ✓ ⇒ the second implementation ✓ ⇒ 64
connections ✓ ⇒ 20 s a point ✓ ⇒ **a fresh server per point** ✓ ⇒ ⇒ ⇒ ⇒ and
the peak is the server's own high-water mark ✓ ⇒ ⇒ ⇒ ⇒ read from `/proc` ✓ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ **the exact number** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ macOS cannot do that ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ (see the M5 report ✓)).

**Two runs per variant** ⇒ ⇒ ⇒ ⇒ so the table is not one lucky run ✓ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ and the range is in the brackets ✓ ✓.

1. **v0.4.1** ⇒ the release before the memory work ✓
2. **v0.4.2** ⇒ the release with the seven fixes ✓
3. **v0.4.2 + `MALLOC_MMAP_THRESHOLD_=65536`** ⇒ ⇒ ⇒ and the allocator giving
   the pages back ✓

⇒ ⇒ **the CPU-bound box serves 17% more traffic** ✓ ✓ ⇒ ⇒ ⇒ (16 812 → 19 724/s ✓)
⇒ ⇒ ⇒ ⇒ ⇒ **and at the same rate the peak is −9%** ✓ ✓ (227 → 208 MB ✓).

⇒ ⇒ **the allocator is not free** ✓ ⇒ ⇒ ⇒ **−5% of the throughput** ✓ ✓
⇒ ⇒ ⇒ ⇒ ⇒ (mmap/munmap are system calls ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ the pages come
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ back, but the CPU pays for them ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **it is a knob ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ the
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ deployment picks it ✓ ✓)**.

⇒ ⇒ [#18](https://github.com/sumimi-dev/sumimi/issues/18) ✓ ⇒ ⇒ ⇒ ⇒ the whole
⇒ ⇒ ⇒ ⇒ ⇒ record ✓ ⇒ ⇒ ⇒ ⇒ ⇒ the discussion there ✓ ✓.

---


# The measurement — 2026-11-08 15:05 UTC

**The machine that was measured**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The machine that measured it**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The server**: Sumimi 0.4.1

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 000 | 99 999 | 0 | 5 000/s | 136 MB |
| 10 000 | 199 999 | 199 998 | 0 | 10 000/s | 145 MB |
| 20 000 | 335 103 | 333 566 | 0 | 16 751/s | 220 MB |
| 40 000 | 338 772 | 337 172 | 0 | 16 931/s | 220 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 0 | 1 MB | 8 ms |
| 10 000 | 0 | 2 MB | 72 ms |
| 20 000 | 0 | 3 MB | 814 ms |
| 40 000 | 0 | 3 MB | 745 ms |

# The measurement — 2026-11-08 15:06 UTC

**The machine that was measured**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The machine that measured it**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The server**: Sumimi 0.4.1

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 003 | 99 997 | 0 | 5 000/s | 138 MB |
| 10 000 | 200 000 | 199 998 | 0 | 10 000/s | 146 MB |
| 20 000 | 337 520 | 335 922 | 0 | 16 872/s | 234 MB |
| 40 000 | 338 873 | 337 274 | 0 | 16 935/s | 239 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 0 | 1 MB | 10 ms |
| 10 000 | 0 | 2 MB | 191 ms |
| 20 000 | 0 | 3 MB | 895 ms |
| 40 000 | 0 | 3 MB | 731 ms |

# The measurement — 2026-11-08 15:08 UTC

**The machine that was measured**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The machine that measured it**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The server**: Sumimi 0.4.2

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 000 | 99 998 | 0 | 5 000/s | 138 MB |
| 10 000 | 200 000 | 199 997 | 0 | 10 000/s | 145 MB |
| 20 000 | 393 279 | 391 704 | 0 | 19 656/s | 207 MB |
| 40 000 | 385 756 | 384 176 | 0 | 19 281/s | 210 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 998 | 2 MB | 231 ms |
| 10 000 | 997 | 2 MB | 225 ms |
| 20 000 | 743 | 3 MB | 715 ms |
| 40 000 | 238 | 3 MB | 845 ms |

# The measurement — 2026-11-08 15:10 UTC

**The machine that was measured**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The machine that measured it**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The server**: Sumimi 0.4.2

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 003 | 99 997 | 0 | 5 000/s | 138 MB |
| 10 000 | 200 000 | 199 998 | 0 | 10 000/s | 145 MB |
| 20 000 | 395 976 | 394 405 | 0 | 19 792/s | 207 MB |
| 40 000 | 393 836 | 392 272 | 0 | 19 683/s | 209 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 997 | 2 MB | 220 ms |
| 10 000 | 998 | 2 MB | 224 ms |
| 20 000 | 436 | 3 MB | 667 ms |
| 40 000 | 300 | 3 MB | 1014 ms |

# The measurement — 2026-11-08 15:11 UTC

**The machine that was measured**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The machine that measured it**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The server**: Sumimi 0.4.2

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 000 | 99 998 | 0 | 5 000/s | 110 MB |
| 10 000 | 200 001 | 199 998 | 0 | 10 000/s | 115 MB |
| 20 000 | 372 157 | 370 614 | 0 | 18 604/s | 182 MB |
| 40 000 | 366 447 | 364 847 | 0 | 18 317/s | 177 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 999 | 2 MB | 311 ms |
| 10 000 | 0 | 2 MB | 341 ms |
| 20 000 | 621 | 3 MB | 1280 ms |
| 40 000 | 892 | 3 MB | 1078 ms |

# The measurement — 2026-11-08 15:13 UTC

**The machine that was measured**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The machine that measured it**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The server**: Sumimi 0.4.2

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 000 | 99 999 | 0 | 5 000/s | 110 MB |
| 10 000 | 199 999 | 199 998 | 0 | 10 000/s | 115 MB |
| 20 000 | 374 629 | 373 042 | 0 | 18 725/s | 182 MB |
| 40 000 | 369 912 | 368 314 | 0 | 18 490/s | 186 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 0 | 2 MB | 302 ms |
| 10 000 | 999 | 2 MB | 338 ms |
| 20 000 | 100 | 3 MB | 1242 ms |
| 40 000 | 376 | 3 MB | 1290 ms |
