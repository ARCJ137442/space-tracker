# MVP 验证记录

本文件记录可公开复现的验证方法。测试路径使用通用占位符，真实快照和本机路径不进入仓库。

## 自动化验证

```text
cargo fmt --all -- --check                         通过
cargo test --all-targets                            通过
cargo clippy --all-targets -- -D warnings          通过
cargo build --release                              通过
```

当前测试覆盖：

- Windows 路径大小写和尾部分隔符规范化；
- 嵌套目录增长和增长速度计算；
- Windows 目录联接点识别、目标不重复计数和目标路径记录；
- `.base.zst` 基线、`.delta.zst` 增量的保存、恢复和旧 JSON 兼容读取；
- manifest 历史摘要、`query` 查询入口和增量链恢复；
- 交互模式的大小阈值解析和系统目录过滤。
- 交互模式的增长目录去重、结果顺序和百分比进度基线。

## 可控夹具验证

对一个小型测试目录执行两次扫描，在两次扫描之间扩大文件并新增文件，同时创建目录联接点：

```powershell
$tool = ".\target\release\space_tracker.exe"
$root = ".\fixtures\fixture-root"
$store = ".\fixtures\snapshots"

& $tool scan $root --store $store --compact --json
# 修改夹具后再次执行
& $tool scan $root --store $store --compact --json
& $tool delta --root $root --store $store --min-bytes 1 --json
& $tool query --root $root --path . --store $store --json
```

预期结果：

- 根目录总大小等于普通文件逻辑长度之和；
- Delta 列出新增文件和所有受影响的父目录；
- 联接点大小为 0，目标内容不会被重复计入；
- `links` 能列出联接点及其目标。

## 交互模式验证

将输入通过标准输入传给无参程序，可以验证向导而不需要 TUI 或 GUI：

```powershell
$input = ".\fixtures\fixture-root`n.\fixtures\snapshots`n100M`n"
$input | & $tool
```

第二次扫描同一路径后，程序应先显示当前占用路径，最后显示两次扫描之间的持续时间和增长提醒；增长提醒只保留增长树最深层的超阈值目录，并按变化量从小到大展示。

## 全盘参考验证

在本地执行全盘验证时，建议把快照放在扫描根目录之外：

```powershell
$tool = ".\target\release\space_tracker.exe"
$root = "C:\"
$store = "D:\space-tracker-snapshots"
& $tool scan $root --store $store --compact --json
```

全盘结果可能包含权限错误、正在变化的文件和 Reparse Point；应重点检查文件/目录数量、错误数、链接数和快照是否成功保存，而不是要求每次扫描的总字节数完全相同。

## 隐私检查

提交前执行：

```powershell
rg -n -i "C:\\Users\\(?!YourName)|[A-Z]:\\[^\r\n`"]+\\space_tracker|snapshot-[0-9]+|\.c-drive|\.real-validation|validation-trash" .
git status --short --ignored
git diff --cached --check
```

如果发现真实用户名、工程绝对路径、快照文件名或本机验证目录，应先移除或改为通用示例，再提交。
