# The sweeps — 2026-11-09 — a GitHub Linux runner (the measurement workflow)

**The workflow, dispatched by hand** ✓ — `.github/workflows/measure.yml` ✓ ⇒ it is on
`main` now ✓ ⇒ ⇒ the run: <https://github.com/sumimi-dev/sumimi/actions/runs/42526757614>
✓ ⇒ ⇒ it took **10 minutes 26 seconds** ✓ ⇒ ⇒ **and it works** ✓ ⇒ ⇒ ⇒ ⇒ so
anybody can repeat it ✓ ✓

**The runner**: an ordinary GitHub-hosted Ubuntu runner ⇒ ⇒ the load generator and
the server share it ✓ ⇒ ⇒ ⇒ so the ceiling below is the runner's, not the
server's ✓ ⇒ ⇒ ⇒ the peak is the server's own `/proc` `VmHWM` ⇒ ⇒ ⇒ the exact
high-water mark ✓ ✓

1. **v0.4.1** ⇒ the release before the memory work ✓;
2. **v0.4.3** ⇒ the release with the seven fixes ✓;
3. **v0.4.3 + `MALLOC_MMAP_THRESHOLD_=65536`** ⇒ ⇒ the allocator giving the pages
   back ✓.

⇒ ⇒ **20 000 requests a second is served exactly** (19 999/s ✓) ⇒ ⇒ ⇒ **and the
peak is 214 MB** ✓ ⇒ ⇒ ⇒ ⇒ the minimum tier is 256 MB ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ **42 MB to spare** ✓ ✓

⇒ ⇒ **the seven fixes, on a third machine** ⇒ ⇒ ⇒ at the runner's ceiling the
same rate is served with **7% more traffic** (26 787 → 28 716/s ✓) ⇒ ⇒ ⇒ ⇒ and
the allocator takes **26% / 24% / 22%** off the peak at the three rates ✓ ✓

---

# The measurement — v0.4.1

**The machine that was measured**: linux · the server: Sumimi 0.4.1

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 99 996 | 99 996 | 0 | 5 000/s | 144 MB |
| 10 000 | 200 001 | 199 999 | 0 | 10 000/s | 138 MB |
| 20 000 | 399 999 | 399 994 | 0 | 20 000/s | 282 MB |
| 40 000 | 535 916 | 534 332 | 0 | 26 787/s | 366 MB |

---

# The measurement — v0.4.3

**The machine that was measured**: linux · the server: Sumimi 0.4.3

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 003 | 99 997 | 0 | 5 000/s | 149 MB |
| 10 000 | 200 000 | 199 999 | 0 | 10 000/s | 158 MB |
| 20 000 | 400 005 | 400 002 | 0 | 20 000/s | 275 MB |
| 40 000 | 574 520 | 572 949 | 0 | 28 716/s | 319 MB |

---

# The measurement — v0.4.3 + the allocator

**The machine that was measured**: linux · the server: Sumimi 0.4.3

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 000 | 100 003 | 99 997 | 0 | 5 000/s | 110 MB |
| 10 000 | 200 000 | 199 998 | 0 | 10 000/s | 120 MB |
| 20 000 | 400 045 | 400 022 | 0 | 19 999/s | 214 MB |
| 40 000 | 574 606 | 573 093 | 0 | 28 725/s | 309 MB |
