# Space Tracker MVP 设计说明

## 1. 设计取舍

MVP 采用标准库递归扫描和 JSON 文件快照。这样可以：

- 不引入数据库和后台服务；
- 让 Agent 能直接读取快照；
- 让失败扫描、权限错误和数据口径可以被审计；
- 把“快照采集”和“快照比较”解耦，后续再替换存储层。

SpaceMan 的 `follow_links(false)` 和并行遍历思路可借鉴，但第一版不引入并行依赖；StorageMaster 的“同根目录找上一份快照 + 对所有目录记录做 Delta”作为增长模型；Python Folder Size Scanner 的后序目录汇总和错误继续扫描作为扫描流程参考。

## 2. 模块边界

```text
src/
├── main.rs       CLI 解析、命令分发、退出码
├── interactive.rs 无参交互向导、进度显示、重点路径和增长提醒
├── model.rs      Snapshot、Entry、Delta 等稳定数据结构
├── scanner.rs    Windows 文件属性、递归扫描、目录汇总
├── store.rs      JSON 快照保存、读取、按根目录查找历史
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
7. 扫描过程中通过节流后的进度回调通知交互层；扫描结束后写入完整快照；写文件采用临时文件后替换，避免留下半份 JSON。

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
