# The sweeps — 2026-11-08 — Linux, 2 vCPU AMD EPYC, 2 GB

**Three sweeps, back to back, on the same box** ⇒ ⇒ ⇒ so they are comparable ✓ ⇒
⇒ ⇒ (the tool: `sumimi-gotest sweep` ✓ ⇒ the second implementation ✓ ⇒ 64
connections ✓ ⇒ 20 s a point ✓ ⇒ **a fresh server per point** ✓ ⇒ ⇒ ⇒ ⇒ and
the peak is the server's own high-water mark ✓ ⇒ ⇒ ⇒ ⇒ read from `/proc` ✓).

1. **v0.4.1** ⇒ the release before the memory work ✓
2. **v0.4.2** ⇒ the release with the seven fixes ✓
3. **v0.4.2 + `MALLOC_MMAP_THRESHOLD_=65536`** ⇒ ⇒ ⇒ and the allocator giving
   the pages back ✓ ⇒ ⇒ ⇒ (**−19% / −21% / −9% / −30%** ✓ ✓)

⇒ ⇒ **the CPU-bound box serves 16% more traffic** ✓ ✓ ⇒ ⇒ ⇒ (the batching:
1 000 transactions became 3 ✓ ⇒ ⇒ ⇒ ⇒ ⇒ CPU was the ceiling ✓ ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ **the ceiling moved** ✓ ✓) ⇒ ⇒ ⇒ **and at the same rate the peak is
−18%** ✓ ✓.

⇒ ⇒ The rule changed ✓ ⇒ ⇒ ⇒ it used to be **68 MB + 7.8 MB per 1 000
requests a second** ✓ ⇒ ⇒ ⇒ ⇒ **now it is 68 MB + 1.2 MB per 1 000** ✓ ⇒ ⇒
⇒ ⇒ ⇒ **six times flatter** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ [#18](https://github.com/sumimi-dev/sumimi/issues/18) ✓ ✓.

---

# The measurement — 2026-11-08 13:27 UTC

**The machine that was measured**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The machine that measured it**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The server**: Sumimi 0.4.1

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 99 995 | 99 995 | 0 | 5 000/s | 132 MB |
| 10 000 | 199 999 | 199 993 | 0 | 10 000/s | 148 MB |
| 20 000 | 342 711 | 341 194 | 0 | 17 131/s | 227 MB |
| 40 000 | 339 190 | 337 624 | 0 | 16 952/s | 201 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 0 | 1 MB | 10 ms |
| 10 000 | 0 | 2 MB | 197 ms |
| 20 000 | 0 | 3 MB | 907 ms |
| 40 000 | 0 | 3 MB | 930 ms |
# The measurement — 2026-11-08 13:28 UTC

**The machine that was measured**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The machine that measured it**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The server**: Sumimi 0.4.2

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 99 999 | 99 999 | 0 | 5 000/s | 136 MB |
| 10 000 | 200 001 | 199 998 | 0 | 10 000/s | 146 MB |
| 20 000 | 397 329 | 395 731 | 0 | 19 861/s | 205 MB |
| 40 000 | 405 391 | 403 791 | 0 | 20 264/s | 261 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 999 | 2 MB | 213 ms |
| 10 000 | 0 | 2 MB | 240 ms |
| 20 000 | 793 | 3 MB | 733 ms |
| 40 000 | 6 855 | 2 MB | 987 ms |
# The measurement — 2026-11-08 13:31 UTC

**The machine that was measured**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The machine that measured it**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The server**: Sumimi 0.4.2

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 99 999 | 99 999 | 0 | 5 000/s | 110 MB |
| 10 000 | 200 000 | 199 998 | 0 | 10 000/s | 116 MB |
| 20 000 | 376 624 | 375 024 | 0 | 18 817/s | 187 MB |
| 40 000 | 377 945 | 376 405 | 0 | 18 894/s | 183 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 999 | 2 MB | 326 ms |
| 10 000 | 999 | 2 MB | 327 ms |
| 20 000 | 88 | 3 MB | 1382 ms |
| 40 000 | 409 | 3 MB | 1213 ms |
