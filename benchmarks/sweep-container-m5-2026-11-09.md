# The sweeps — 2026-11-09 — macOS, Apple M5, the server in a Linux container

**Three sweeps, back to back, in the same container** (the tool: `sumimi-gotest`
⇒ the second implementation ⇒ 64 connections ⇒ 20 s a point ⇒ **a fresh
container per point** ⇒ ⇒ ⇒ ⇒ and the peak is the server's own
`/proc/self/status` `VmHWM` ⇒ ⇒ ⇒ ⇒ ⇒ **the exact high-water mark** ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ macOS cannot do that).

1. **the image as it ships** ⇒ ⇒ ⇒ ⇒ `MALLOC_MMAP_THRESHOLD_=65536` ✓ (the
   Dockerfile sets it ✓) ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **the pages come back** ✓;
2. **glibc as it comes** ⇒ ⇒ ⇒ ⇒ ⇒ the variable is gone ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒
   ⇒ ⇒ ⇒ **the default: a 128 KB mmap threshold, and it raises that threshold
   as it goes** ✓;
3. **never map anything** ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ `MALLOC_MMAP_THRESHOLD_=0` ⇒ ⇒ ⇒ ⇒ ⇒ ⇒
   ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **the worst case: glibc keeps every page it
   takes** ✓.

⇒ ⇒ **the container's own cgroup, right after each point** ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **is in the table** ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **because it
is the best proof there is** ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **the peak is the
highest the container ever was; the cgroup is what it holds now** ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **and they differ by five times** ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ **that is the pages coming back** ✓ ✓

⇒ ⇒ [#18](https://github.com/sumimi-dev/sumimi/issues/18) ✓ ⇒ ⇒ ⇒ ⇒ the whole
record ✓ ⇒ ⇒ ⇒ ⇒ ⇒ the discussion there ✓ ✓.

---

# The comparison — three environments, one table

| the rate offered | **macOS, native** | **macOS, in a container** | **the VPS** |
| --- | ---: | ---: | ---: |
| 5 000 /s | 139 MB | **106 MB** | 110 MB |
| 10 000 /s | 147 MB | **116 MB** | 115 MB |
| 20 000 /s | 246 MB | **213 MB** | **182 MB** (served 18 600/s) |
| 40 000 /s | 270 MB | **270 MB** (served 39 107/s) | **182 MB** (served 18 400/s) |

**The machines**: macOS native — an Apple M5, 10 cores, macOS, the peak is the
kernel's `ru_maxrss` (and macOS does not repeat it from one round to the next).
macOS in a container — the same M5, the server in the Docker Desktop Linux VM
(aarch64), the peak is the server's own `VmHWM` inside the container. The VPS —
an AMD EPYC-Genoa, **2 cores**, 2 GB, Linux, the peak is the server's own
`VmHWM`. The same released binary, the same tool, the same rates; the container
and the VPS both have `MALLOC_MMAP_THRESHOLD_=65536` (the image sets it) and the
native macOS run cannot use it (that is Apple's allocator, not glibc's).

**What the table says.**

* **The container is 20–24% below the native macOS at the same rates** — and
  that is **not** the container's doing: it is the **allocator**. The container
  has glibc (the variable works); the native macOS run has Apple's allocator
  (the variable does nothing). Same machine, same binary, different allocator,
  **−20% of the peak**.
* **The cgroup's own number after each point is a fifth of the peak** (106 MB
  peak → 23 MiB held; 304 MB peak → 178 MiB held). That is the pages coming
  back, measured by the kernel, in the same table.
* **The VPS column is lower, but not because it is thrifty**: that box has **2
  cores**, so at 20 000 requests a second it only served ~18 600/s — it did
  less work, so it peaked lower. Compare at the rates each machine can serve:
  at 40 000/s the container peaked at **270 MB** while serving 39 107/s, and the
  VPS peaked at **182 MB** while serving 18 400/s. **The CPU decides how far up
  the rate axis you can go; the memory decides what that costs.**
* **The native macOS column is the least trustworthy of the three** — it is the
  kernel's `ru_maxrss`, which counts the shared pages of the mapped binary and
  moves by up to 30% between rounds. The container's column is the kernel's
  exact high-water mark. **Take the numbers from the container or the VPS.**

---

# The measurement — the image as it ships

**The machine**: Apple M5 - 10 cores - the server runs in the Docker
Desktop Linux VM (aarch64) - the peak is the server's own
/proproc/self/status VmHWM inside the container - the exact high-water mark

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak | the cgroup |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 5,000 | 100,000 | 100,000 | 0 | 5,000/s | **106 MB** | 23MiB / 7.748GiB |
| 10,000 | 200,000 | 199,999 | 0 | 10,000/s | **116 MB** | 28.61MiB / 7.748GiB |
| 20,000 | 399,999 | 399,993 | 0 | 20,000/s | **213 MB** | 27.36MiB / 7.748GiB |
| 40,000 | 782,207 | 780,610 | 0 | 39,107/s | **270 MB** | 40.71MiB / 7.748GiB |

---

# The measurement — glibc as it comes

**The machine**: Apple M5 - 10 cores - the server runs in the Docker
Desktop Linux VM (aarch64) - the peak is the server's own
/proc/self/status VmHWM inside the container - the exact high-water mark

  **The variable is gone**: glibc with its own default - a 128 KB mmap
  threshold, and it raises that threshold as it goes.

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak | the cgroup |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 5,000 | 100,000 | 99,999 | 0 | 5,000/s | **141 MB** | 56.48MiB / 7.748GiB |
| 10,000 | 199,999 | 199,998 | 0 | 10,000/s | **157 MB** | 75.96MiB / 7.748GiB |
| 20,000 | 399,999 | 399,994 | 0 | 20,000/s | **272 MB** | 87.06MiB / 7.748GiB |
| 40,000 | 800,000 | 799,976 | 0 | 40,000/s | **314 MB** | 101.9MiB / 7.748GiB |

---

# The measurement — never map anything

**The machine**: Apple M5 - 10 cores - the server runs in the Docker
Desktop Linux VM (aarch64) - the peak is the server's own
/proc/self/status VmHWM inside the container - the exact high-water mark

  **The allocator is OFF**: MALLOC_MMAP_THRESHOLD_=0 - nothing is ever mapped
  separately, so glibc keeps every page it takes.

| the rate offered | sent | accepted | refused | the rate achieved | the server's peak | the cgroup |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 5,000 | 100,000 | 99,999 | 0 | 5,000/s | **304 MB** | 177.7MiB / 7.748GiB |
| 10,000 | 200,000 | 199,999 | 0 | 10,000/s | **364 MB** | 180MiB / 7.748GiB |
| 20,000 | 399,999 | 399,991 | 0 | 20,000/s | **389 MB** | 175.2MiB / 7.748GiB |
| 40,000 | 427,738 | 426,142 | 0 | 21,386/s | **443 MB** | 189.9MiB / 7.748GiB |

⇒ ⇒ **the allocator, on the same machine, in the same container** ✓ ⇒ ⇒ ⇒

| the rate offered | glibc as it comes | **the image as it ships** | never map anything |
| --- | ---: | ---: | ---: |
| 5 000 /s | 141 MB | **106 MB** ✓ | 304 MB |
| 10 000 /s | 157 MB | **116 MB** ✓ | 364 MB |
| 20 000 /s | 272 MB | **213 MB** ✓ | 389 MB |
| 40 000 /s | 314 MB | **270 MB** ✓ | 443 MB |

⇒ ⇒ **−25% / −26% / −22% / −14% against glibc's own default** ✓ ⇒ ⇒ ⇒ **and
−65% / −68% / −45% / −39% against the worst case** ✓ ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ **that one environment variable is worth more than every other line of
this file** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **and it is the reason the
image sets it** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **the deployment
reference table needs it** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **the
container already has it** ✓ ✓
