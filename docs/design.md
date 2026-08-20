# Space Tracker MVP 设计说明

## 1. 设计取舍

MVP 采用标准库递归扫描和 zstd 压缩快照。压缩流内部继续使用 serde JSON，便于复用已有结构；磁盘上按基线/增量保存，不再为每次扫描写一份完整 JSON。这样可以：

- 不引入数据库和后台服务；
- 让 Agent 能通过 CLI 查询快照；
- 让失败扫描、权限错误和数据口径可以被审计；
- 把“快照采集”和“快照比较”解耦，后续可替换压缩流内部载荷。

SpaceMan 的 `follow_links(false)` 和并行遍历思路可借鉴；当前实现使用 Rayon 对宽目录拆分独立扫描任务，并在局部结果归并后生成统一快照。StorageMaster 的“同根目录找上一份快照 + 对所有目录记录做 Delta”作为增长模型；Python Folder Size Scanner 的后序目录汇总和错误继续扫描作为扫描流程参考。

## 2. 模块边界

```text
src/
├── main.rs       CLI 解析、命令分发、退出码
├── interactive.rs 无参交互向导、进度显示、重点路径和增长提醒
├── model.rs      Snapshot、Entry、Delta 等稳定数据结构
├── scanner.rs    Windows 文件属性、递归扫描、目录汇总
├── store.rs      manifest、压缩 base/delta 保存读取、按根目录查找历史
├── delta.rs      两次快照的路径连接、增长排序和速度计算
└── output.rs     人类可读输出与 JSON 输出
```

每个模块都尽量接收普通结构并返回 `Result`，便于单元测试；CLI 和交互层不直接承担扫描算法。

## 3. 快照模型

```text
Snapshot
├── format_version
├── snapshot_id
├── root
├── root_key
├── captured_at_unix_ms
├── duration_ms
├── total_bytes
├── file_count
├── directory_count
├── reparse_point_count
├── errors[]
└── entries[]
    ├── relative_path
    ├── kind: file | directory
    ├── size_bytes
    ├── modified_unix_ms
    ├── is_reparse_point
    ├── link_target
    └── followed: false
```

目录条目的 `size_bytes` 是递归总大小；链接目录条目的大小为 0，避免把目标内容重复计算。链接仍保留在 `entries` 中，所以可以被 `links` 单独列出。

## 4. 扫描算法

1. 对根路径使用 `symlink_metadata`，不直接跟随链接。
2. Windows 下检查 `FILE_ATTRIBUTE_REPARSE_POINT`；其他平台退化为 `FileType::is_symlink()`。
3. 若条目是 Reparse Point：记录条目、尝试解析目标、停止向下遍历。
4. 若条目是普通文件：读取长度和修改时间，加入文件条目和当前目录的直接大小。
5. 若条目是普通目录：递归处理，并把子目录总大小回加到父目录。
6. 目录读取或元数据读取失败时写入错误列表，继续处理同级其他项。
7. 有参扫描将宽目录拆成 Rayon 独立任务，各任务维护局部条目、错误和计数后归并；交互式正式扫描保留顺序进度回调，扫描前的进度基线使用同样的受控并行策略。
8. 扫描过程中通过节流后的进度回调通知交互层；扫描结束后写入完整快照；存储层采用临时文件后替换，避免留下半份快照。

## 5. Delta 算法

以 `relative_path + kind` 作为条目身份：

- 当前目录存在、旧目录存在：`delta = current.size - previous.size`；
- 当前文件存在、旧文件不存在：归入新增文件；
- 旧文件存在、当前文件不存在：归入删除文件；
- 目录 Delta 保留所有层级，因此 `AppData\Local\Microsoft\...` 可以单独出现在结果中；
- 速度使用两次快照的时间差折算为 bytes/day；时间差为零时速度为 `null`；
- 根目录不一致时拒绝比较。

## 6. 输出契约

人类输出服务于快速判断，JSON 输出服务于 Agent。JSON 字段名使用 snake_case，大小统一用整数 bytes，避免解析格式化字符串。

示意：

```json
{
  "root": "C:\\Users\\YourName\\AppData",
  "previous_snapshot": "...",
  "current_snapshot": "...",
  "elapsed_ms": 86400000,
  "growing_directories": [
    {
      "relative_path": "Local\\Microsoft",
      "current_bytes": 8912345678,
      "previous_bytes": 8123456789,
      "delta_bytes": 788888889,
      "delta_percent": 9.71,
      "growth_per_day_bytes": 788888889
    }
  ],
  "new_files": [],
  "removed_files": []
}
```

## 7. 风险和边界

- 目录大小是逻辑文件长度之和，不是 NTFS 占用空间；暂不处理压缩、稀疏文件、硬链接去重。
- 权限不足会使结果小于真实值，因此必须输出错误数和错误路径。
- 扫描中途文件可能被修改，快照是一个时间窗口内的近似观察，不是事务快照。
- 默认不跟随 Reparse Point；未来若支持跟随，必须增加目标身份去重和循环保护。
- 当前不提供删除、迁移和建链操作，避免把只读观察工具过早变成高风险文件操作工具。

## 8. 快照存储与查询

用户指定数据目录后，存储层会创建一个 `space-tracker` 子目录，并按根目录指纹分开保存：

```text
<data-dir>\space-tracker\<root-fingerprint>\
├── manifest.json
├── snapshot-<time>.base.zst
└── snapshot-<time>.delta.zst
```

第一次扫描写 `.base.zst`，内容是完整 `Snapshot`；后续扫描与最近一次逻辑快照比较，只把新增、删除和字段发生变化的条目写入 `.delta.zst`。增量还包含当前扫描的摘要、错误列表和时间戳，所以可以从最近基线按时间顺序恢复已保存的逻辑快照。

为避免增量链无限变长，默认最多连续保存 32 个 delta；达到上限后下一次扫描自动写新的 base，旧基线和旧增量不删除。

`history` 只读取 manifest 摘要，不解压大快照；`delta`、`children`、`query` 和 `links` 在读取 `.base.zst`/`.delta.zst` 时恢复逻辑快照，因此原有 CLI 输出契约保持不变。写入采用临时文件、flush、zstd 流结束后原子 rename，manifest 单独原子更新。

旧版直接存放的 `.json` 快照仍可读取；当新链不存在时，下一次扫描会创建新基线，不删除旧文件。
