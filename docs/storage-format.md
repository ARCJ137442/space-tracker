# 快照存储格式选型

## 目标

当前扫描器内存中的核心结构是：一次 `Snapshot` 包含一个 `Vec<Entry>`，每个条目包含相对路径、类型、逻辑大小、修改时间、Reparse Point 标记、链接目标和跟随状态。一次全盘扫描会产生近百万条目；旧版紧凑 JSON 快照约为 300MB 量级。

本次存储层优化优先解决四件事：

1. 后续扫描不再重复写完整快照。
2. 首份基线也要明显小于原始 JSON。
3. 保留 `history`、`delta`、`children`、`links` 和 Agent 查询能力。
4. 写入过程可恢复，不能留下半份快照；格式不应依赖 .NET 或后台服务。

## 候选比较

| 候选 | 优点 | 主要代价 | 当前结论 |
|---|---|---|---|
| zstd 压缩的现有 serde 结构 | 改动小、跨平台、直接复用 `Snapshot/Entry`、压缩比高、容易做 `.base`/`.delta` | 查询时需要解压并反序列化基线；没有磁盘索引 | 第一阶段采用 |
| SQLite | SQL、索引、事务和任意查询成熟；适合做当前状态表 | 基线表和索引会重复保存路径；增量文件、链合并和 Windows 打包需要额外设计 | 后续“索引查询版”候选 |
| redb | 纯 Rust、嵌入式、ACID、Copy-on-Write B+Tree、范围查询和零拷贝值访问 | 没有 SQL；需要自行设计路径键、变更链和压缩；多文件 delta 查询层仍需维护 | 适合自定义索引版，不作为第一步 |
| rkyv | 面向 Rust 结构的零拷贝归档，读取时可避免完整反序列化 | 仍需把归档映射/载入内存；不自带按目录和路径的查询索引 | 适合第二阶段替换载荷，不单独解决查询模型 |
| Parquet/Arrow | 列式压缩和分析读取强 | 小规模路径变更、树形下钻和逐条删除并非主要优化目标 | 不适合当前快照链 |
| bincode 等通用二进制序列化 | 比 JSON 更紧凑、编码解码简单 | 只负责序列化，不提供索引、事务或增量语义；生态维护边界需要额外评估 | 不作为独立存储层 |

资料依据：SQLite 的数据库文件由页、B-tree 和索引组成，支持事务和 WAL；SQLite 官方也提供 Session Extension 来记录 changeset/patchset，但它要求另一端具有兼容 schema 和起始数据。[SQLite 文件格式](https://www.sqlite.org/fileformat.html)、[SQLite 查询规划](https://www.sqlite.org/queryplanner.html)、[SQLite Session Extension](https://www.sqlite.org/sessionintro.html)。redb 官方项目强调纯 Rust、ACID、MVCC、Copy-on-Write B+Tree 和稳定文件格式。[redb](https://github.com/cberner/redb)。rkyv 官方定位是 Rust 的 zero-copy deserialization，但归档仍需要载入其字节缓冲区。[rkyv](https://rkyv.org/zero-copy-deserialization.html)。Parquet 官方格式是列式文件，文件尾部元数据定位列块，适合顺序分析读取。[Parquet 文件格式](https://parquet.apache.org/docs/file-format/)。

## 第一阶段方案

第一阶段使用 zstd 压缩的 JSON 载荷，但 JSON 只存在于压缩流内部，不再作为主要的可读交换格式：

```text
<data-dir>\space-tracker\<root-fingerprint>\
├── manifest.json
├── snapshot-<time>.base.zst
└── snapshot-<time>.delta.zst
```

- `.base.zst`：保存某个根目录的完整 `Snapshot`。
- `.delta.zst`：保存下一次扫描的摘要字段、错误列表和条目变更；变更是 `upsert` 或 `delete`。
- `manifest.json`：保存根目录、根指纹、文件类型和摘要，不需要为了 `history` 解压基线。
- 根目录文件夹使用稳定的非隐私指纹命名；完整根路径只在本地 manifest 和快照数据中保存。
- 写入先写临时文件，再 flush、结束 zstd 流并原子 rename；manifest 也采用相同策略。
- 旧 `.json` 仍可读。若新存储链尚不存在，下一次扫描建立新的 `.base.zst`，不删除旧文件。
- 增量链默认最多保留 32 个 delta；达到上限时下一次扫描自动重建一个新的 base，旧文件仍保留以支持历史查询。

这套方案充分利用了当前结构的两个事实：快照条目本身已经是稳定的 serde 数据结构，且连续扫描之间大多数条目不变。它把“压缩重复文本”和“只记录变化”分开处理，因此第一次扫描和后续扫描分别有明确的读写策略。

## 实测结果

在当前 Windows 环境以 Release 构建扫描 `C:\`：

- 基线：597,119 个文件、145,021 个目录、125 个错误，遍历约 67.5 秒。
- `.base.zst`：11,566,963 字节。
- 紧接着第二次扫描：`.delta.zst` 为 6,769 字节；该次扫描发现文件数变化很小，增量仍远小于完整快照。
- `history` 只读 manifest；`delta` 和 `query` 会恢复逻辑快照。

## 已知代价与下一步

当前 `delta`/`query` 为了兼容现有 `Snapshot` API，会解压并反序列化基线，再叠加增量。在本机约百万条目规模下，示例查询耗时约数秒；这已经把磁盘写入成本降到可接受范围，但还不是最终的随机查询最优解。

下一阶段若查询耗时成为主要问题，优先顺序是：

1. 保持 `.base.zst`/`.delta.zst` 文件契约不变，把内部载荷改成版本化的二进制记录。
2. 基线按目录树存储 `parent_id + name_id`，对重复目录名和路径前缀做字典化。
3. 为基线增加父目录范围索引或块索引，使 `children` 不必恢复整个快照。
4. 在现有数量阈值之外增加累计大小阈值，并提供可显式触发的安全 base 重建；重建前后保留 manifest 可验证性。

SQLite 或 redb 只有在确实需要复杂筛选、跨快照 SQL 或高并发查询时再引入。它们可能改善查询，但不应为了查询能力重新制造接近 200MB 的单次全量写入。
