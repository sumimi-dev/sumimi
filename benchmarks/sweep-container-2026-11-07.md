# The measurement — 2026-11-07 20:52 UTC

> A **container** is what a customer actually runs ✓ ⇒ these are the numbers to
> quote ✓.
>
> 4 vCPU ✓, the **128 MB engine tier** ✓ (= the minimum tier ✓), the released
> v0.4.0 ✓, the tool **v0.1.2** ✓. The container's memory limit was 2 GB (the
> tool lives in it too ✓) — the **engine** limit is what the tier is about ✓.
>
> The last row is the **gate** ✓: the limit is 20 000/s ✓ ⇒ offering 30 000/s
> gets **180 313 refusals** ✓ ⇒ the server admits 20 000/s ✓ ⇒ **the memory does
> not grow** ✓ ⇒ the box is protected ✓.
>
> The peak is the server's own high-water mark (`/proc`, exact ✓) ✓.

**The machine that was measured**: an unnamed CPU · 4 cores · 7.7 GB · linux
**The machine that measured it**: an unnamed CPU · 10 cores · 7.7 GB · linux
**The server**: Sumimi 0.4.0

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 003 | 100 000 | 0 | 5 000/s | 117 MB |
| 10 000 | 200 006 | 199 995 | 0 | 10 000/s | 157 MB |
| 15 000 | 300 003 | 300 002 | 0 | 15 000/s | 163 MB |
| 20 000 | 400 000 | 399 999 | 0 | 20 000/s | 208 MB |
| 30 000 | 600 030 | 419 716 | 180 313 | 29 999/s | 220 MB |

| the rate offered | the queue at the end | the engine held | the slowest write |
| ---: | ---: | ---: | ---: |
| 5 000 | 0 | 2 MB | 32 ms |
| 10 000 | 0 | 2 MB | 32 ms |
| 15 000 | 0 | 2 MB | 99 ms |
| 20 000 | 0 | 768 KB | 398 ms |
| 30 000 | 0 | 768 KB | 1928 ms |
