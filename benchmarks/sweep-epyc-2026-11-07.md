# The measurement — 2026-11-07 20:52 UTC

> The **third machine** ✓ — the one [issue #15](https://github.com/sumimi-dev/sumimi/issues/15)
> asked for ✓: a plain small VPS, a different CPU from the Xeon and the M5 ✓.
>
> Measured with the **released v0.4.0 binary** ✓ (downloaded from the release ✓ —
> **nothing was compiled on that machine** ✓) and the tool in
> [sumimi-gotest](https://github.com/sumimi-dev/sumimi-gotest) ✓ **v0.1.2** ✓
> (the version whose own memory stays bounded ✓ — the earlier run of this sweep
> used a tool that grew to hundreds of megabytes and was OOM-killed in a
> 512 MB container ✓).
>
> The peak is the server's own high-water mark (`/proc`, exact ✓) ✓.
>
> Its ceiling is **~16 700/s** ✓ — two slow cores ✓ — and **past it the memory
> stops growing** ✓ ✓ (the queue is 0 ✓: the requests it cannot parse never
> enter ✓).

**The machine that was measured**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The machine that measured it**: AMD EPYC-Genoa Processor · 2 cores · 2.0 GB · linux
**The server**: Sumimi 0.4.0

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 003 | 99 996 | 0 | 5 000/s | 100 MB |
| 10 000 | 199 997 | 199 996 | 0 | 10 000/s | 143 MB |
| 15 000 | 300 001 | 299 983 | 0 | 15 000/s | 207 MB |
| 20 000 | 334 959 | 333 359 | 0 | 16 739/s | 236 MB |
| 30 000 | 315 049 | 313 449 | 0 | 15 745/s | 223 MB |
| 40 000 | 334 146 | 332 607 | 0 | 16 704/s | 225 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 0 | 2 MB | 8 ms |
| 10 000 | 0 | 2 MB | 71 ms |
| 15 000 | 0 | 2 MB | 790 ms |
| 20 000 | 0 | 3 MB | 934 ms |
| 30 000 | 0 | 3 MB | 983 ms |
| 40 000 | 0 | 2 MB | 912 ms |
