# Space Tracker

Space Tracker 是一个面向 Windows 的只读磁盘空间追踪工具：它不只告诉你“现在谁占空间”，还告诉你“从上次扫描到现在，谁增长了多少”。程序使用 Rust 编写，不依赖大型 TUI 或 .NET 运行时。

## 为什么需要它

传统磁盘分析器擅长生成一张静态空间地图，但清理磁盘时真正想知道的是：

> 我上次已经看过这些目录了。现在重新打开工具时，哪些目录在这段时间里明显变大，增长发生在哪里，是否值得清理、迁移或建立链接？

Space Tracker 围绕这个用户叙事设计：

1. 第一次扫描目标目录并保存快照。
2. 之后再次扫描同一个目录。
3. 把“当前占用”和“增长占用”分成两个视图。
4. 对增长目录继续下钻到子目录和新增文件。
5. 忽略长期稳定的 Windows 和程序安装目录，把注意力放在更可能可处理的用户数据、缓存和开发产物上。

## 特性

- 无参数启动：简单的交互式 CMD 向导。
- 扫描过程中动态显示耗时、文件数、目录数、累计大小、联接点和错误数。
- 交互模式默认扫描 `C:\`，默认把数据放在 exe 所在目录。
- 增长提醒阈值默认 `100M`，支持 `100M`、`1G` 或字节数。
- 扫描进度先建立条目基线并显示百分比；增长提醒按变化量从小到大呈现，并隐藏已被超阈值子目录覆盖的父目录。
- 交互式结果先展示当前占用重点路径，最后固定输出增长提醒。
- 无参模式结束后提示“按任意键退出”，适合直接双击 exe 使用；带参数调用不暂停。
- 识别符号链接、目录联接点等 Windows Reparse Point，默认不跟随，避免重复计数。
- 有参扫描与交互式进度基线使用 Rayon 受控并行；默认线程数不超过 8，可用 `RAYON_NUM_THREADS` 覆盖。
- 有参模式提供稳定的 CLI 和 JSON 输出，便于 Agent、脚本和自动化任务调用。
- 只读设计：不删除、不移动、不创建符号链接。

## 构建

需要 Rust stable：

```powershell
cargo build --release
```

可执行文件位于 `target\release\space_tracker.exe`。压缩快照使用 Rust 的 zstd crate；它不要求安装 .NET Desktop Runtime。

## 人类使用

直接运行：

```powershell
.\target\release\space_tracker.exe
```

依次输入：

```text
扫描路径             [默认 C:\]
数据存放路径         [默认 exe 所在目录]
增长提示阈值         [默认 100M]
```

直接回车接受默认值。程序会在数据存放路径下创建 `space-tracker` 子目录，再按扫描根目录分开保存快照。第二次扫描同一路径后，程序会显示两次快照之间的时间段、超过阈值的增长目录和增长大小。

无参模式结束时会等待按下任意键再退出，避免直接双击 exe 时窗口立即关闭；从命令行带参数调用时保持原有 CLI 行为。

## Agent / CLI 使用

带参数时不会进入交互模式：

```powershell
$tool = ".\target\release\space_tracker.exe"
$store = "D:\space-tracker-snapshots"
$root = "C:\Users\YourName\AppData"

& $tool scan $root --store $store --compact --json
& $tool history --root $root --store $store --json
& $tool delta --root $root --store $store --min-bytes 104857600 --json
& $tool children --root $root --path . --store $store --json
& $tool query --root $root --path AppData --store $store --limit 50 --json
& $tool links --root $root --store $store --json
```

命令说明：

- `scan`：扫描并保存压缩快照；默认不跟随 Reparse Point。`--compact` 保留用于兼容旧调用，新格式始终使用 zstd 压缩。
- `history`：只读取 manifest 摘要，列出同一存储目录中的历史快照。
- `delta`：比较同一根目录的两次快照，输出增长/缩小目录、新增文件和删除文件。
- `children`：查看指定目录的直接子目录和文件。
- `query`：面向 `.base.zst`/`.delta.zst` 的显式查询入口，按路径列出直接子项；`children` 继续作为兼容命令保留。
- `links`：列出识别到的 Reparse Point、目标路径和未跟随状态。

## 快照存储格式

指定 `--store D:\space-tracker-data` 后，实际数据位于：

```text
D:\space-tracker-data\space-tracker\<root-fingerprint>\
├── manifest.json
├── snapshot-<time>.base.zst
└── snapshot-<time>.delta.zst
```

同一根目录的第一次扫描写一个完整 `.base.zst`；后续扫描只保存新增、删除和发生字段变化的条目到 `.delta.zst`。查询时按基线叠加增量恢复目标时刻的逻辑快照。基线和增量载荷沿用 Rust 当前 `Snapshot`/`Entry` 数据结构，只是使用 zstd 压缩；这样可以先消除重复完整快照的磁盘开销，并为后续索引化二进制载荷保留兼容边界。

为避免长期运行后增量链无限变长，连续 32 个 delta 后下一次扫描会自动建立新的 base；旧文件不会被自动删除。

旧版 `.json` 快照仍可读取。若存储目录中只有旧格式，下一次新扫描会建立新的 `.base.zst`，不会删除旧 JSON。

## 隐私与快照

快照可能包含扫描根路径、链接目标和无法访问的文件路径，因此默认只应保存在自己的机器上，不要直接上传快照。仓库已经通过 `.gitignore` 排除快照、验证目录和构建产物；公开文档只使用通用路径示例，不包含开发机用户名或本地工程路径。

## 当前边界

目录大小是可访问普通文件的逻辑长度之和，不等同于 NTFS 实际占用空间；权限错误会保存在快照中。当前版本不处理硬链接去重、压缩文件、稀疏文件的物理占用，也不执行迁移和建链操作。

## AI 使用披露

本项目的主要开发者是 GPT-5.6 Luna。AI 参与了需求整理、架构设计、Rust 代码实现、测试、性能分析、文档编写和发布准备；项目维护者负责提出需求、提供实际使用反馈、审阅结果并决定是否采用和发布相关改动。

## 文档

- [`docs/requirements.md`](docs/requirements.md)：需求、用户叙事和验收标准
- [`docs/design.md`](docs/design.md)：模块边界、扫描算法和快照模型
- [`docs/storage-format.md`](docs/storage-format.md)：存储格式候选比较、选择依据和演进路线
- [`docs/performance.md`](docs/performance.md)：匿名化性能测试与优化记录
- [`docs/validation.md`](docs/validation.md)：自动化、夹具和真实目录验证方法
- [`docs/privacy.md`](docs/privacy.md)：快照隐私边界和公开仓库注意事项
