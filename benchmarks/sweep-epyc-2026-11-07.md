# The measurement — 2026-11-07 19:00 UTC

> The **third machine** ✓ — the one [issue #15](https://github.com/sumimi-dev/sumimi/issues/15)
> asked for ✓: a plain small VPS, a different CPU from the Xeon and the M5 ✓.
>
> Measured with the **released v0.4.0 binary** ✓ (downloaded from the release ✓ —
> **nothing was compiled on that machine** ✓) and the tool in
> [sumimi-gotest](https://github.com/sumimi-dev/sumimi-gotest) ✓.
>
> The peak is the server's own high-water mark (`/proc`, exact ✓) ✓.

**The machine that was measured**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The machine that measured it**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The server**: Sumimi 0.4.0

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 000 | 99 999 | 0 | 5 000/s | 103 MB |
| 10 000 | 200 005 | 199 992 | 0 | 10 000/s | 143 MB |
| 15 000 | 299 975 | 299 935 | 0 | 14 997/s | 202 MB |
| 20 000 | 333 632 | 332 113 | 0 | 16 676/s | 214 MB |
| 30 000 | 333 511 | 331 911 | 0 | 16 669/s | 215 MB |
| 40 000 | 342 158 | 340 573 | 0 | 17 097/s | 197 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 0 | 2 MB | 9 ms |
| 10 000 | 0 | 2 MB | 197 ms |
| 15 000 | 0 | 2 MB | 722 ms |
| 20 000 | 0 | 3 MB | 941 ms |
| 30 000 | 0 | 3 MB | 1158 ms |
| 40 000 | 0 | 3 MB | 885 ms |
