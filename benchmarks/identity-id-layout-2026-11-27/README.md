# 身份两列（session_id / visit_id）的形状 —— 原始测量输出（2026-11-26 夜 ~ 2026-11-27 凌晨）

工具：`service/examples/identity_scale.rs`（本仓）。一次运行量四档形状或一个混合档，
把每个形状落成自己的库文件，再按「去掉某一列再比一次」量每一列的成本。

```bash
cargo build --release --example identity_scale

# 访客列的载体那一格（E=4，取 7 次里最好的一次）
./target/release/examples/identity_scale \
  --rows 2000000 --visitors 200000 --events-per-visit 4 --interleave 4 \
  --threads 4 --no-parquet --repeats 7 \
  --layouts B,B_blob,blob12_bvisit,blob8_bvisit,today

# 每次访问几个事件 × 相邻还是交错
./target/release/examples/identity_scale --rows 2000000 --visitors 200000 \
  --events-per-visit 2 --interleave 4 --threads 4 --no-parquet \
  --layouts today,blob16marker,B,B_blob2

# 计数器的步长（隔 2 隔 3）与回跳深度
./target/release/examples/identity_scale --rows 2000000 --visitors 200000 \
  --events-per-visit 4 --interleave 1 --visit-stride 7 --threads 4 --no-parquet \
  --layouts B,B_blob2

# 多站（写路径按站分组，站内计数器各自从零开始）
./target/release/examples/identity_scale --rows 2000000 --visitors 200000 \
  --events-per-visit 4 --interleave 4 --sites 64 --threads 4 --no-parquet --layouts B
```

**读这些文件之前先看两条**：

1. **盘上的字节数是准的**（不是计时）；**cpu 的数在这台机器上有 ±20% 到 30% 的噪声**
   （量的时候 load average 在 37 到 54 之间，16 核上还有别人的 node 进程在 100% CPU）。
   同一份数据同一天里也出现过 `distinct` 5.0 与 9.1 ms 两个数。跨运行的比值只信 30% 以上的差。
2. **模型不是真实流量**：每一行都是 pageview（真实的一次访问里有 pv ＋ 自定义事件 ＋ 性能事件）；
   访客数与「每访客几行」在模型里绑着；相邻那一档（`interleave = 1`）是模型送的便宜。

## 文件

| 目录 | 是什么 |
|---|---|
| `stock/` | 库存版 DuckDB 2.0.0（`libduckdb-sys` 1.20000.1 的 bundled 构建） |
| `patched/` | 同一个引擎，把**字符串的内联上限从 12 字节改成 16 字节**（六处改动，见 mneme#177）。**它把正好 16 字节的字符串弄坏了** —— 那一档的数据不可用 |

| 文件 | 是什么 |
|---|---|
| `stock/carriers_E{2,4,8}.txt` | 访客列的载体那一格（`B` / `B_blob` / `blob12_bvisit` / `blob8_bvisit` / `today`），`--repeats 7` |
| `stock/carriers_E4_after_restore.txt` | 还原 tarball 并重编之后的对照复测（盘上的数与改之前一模一样） |
| `stock/sweep_E{1,2,4,8}_I{1,4}.txt` | 每次访问几个事件 × 相邻还是交错 |
| `stock/mix_E{2,4,8}.txt` | 混合档（`B` / `B_blob` / `blob12_int4` / `B_blob2` / `today`） |
| `stock/stride_E{1,4}_S{1,2,7}.txt` | 计数器的步长（隔 2 隔 3） |
| `stock/jump_I{2,16,64}.txt` | 回跳深度 |
| `stock/sites_S{1,2,8,64}.txt` | 多站 |

`patched/` 里是同一批配置，文件名相同。

## 每一份输出里有什么

- 第一张表：整行 B/行、去掉两个 id 列后的 B/行、id 两列的成本、写盘秒数。
- 第二张表：`scan`（count(*) 的元数据快路径）、`scanhash`（`sum(hash(session_id))`，纯读）、
  `distinct`（`count(distinct session_id)`）、`summary`（产品那条语句）、
  `summary×2`（拆成两条语句）、`bounces`、`summary30d`。
- 第三张表：`breakdown`、`event log`、内存、溢写、访客数。
- `how the engine stores`：两列各用了什么压法。
- 最后一块：每列的成本（去掉那一列再比一次）。
