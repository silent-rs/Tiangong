# tiangong-memory-consolidator

记忆深度整理**独立工具**。对天工 Memory 的长期记忆库做一次全量「深刻整理」：
归一重复记忆、把零散的工具日志提炼为经历、合并高度一致的记忆、重新归纳分类、
重建关联链，并遗忘低质量记忆。

本工具是独立制品：只通过 `tiangong-memory` 的公开 API 读写记忆（Memory 侧仅为
本工具提供 `delete_node`、`upsert_node_vectors` 与只读的 `evidence_paths` 三个通用能力）；接入方式与 `tiangong-memory-sidecar` 的通用模式
一致（竞争 Leader，天工运行时以 Follower 经 IPC 转发），因此天工正在使用时
也可以安全运行，记忆数据始终只有一份。

## 使用

只需要指定天工根目录，工具会自动识别 Memory 系统配置的模型
（LLM / Embedding / Rerank）：

```bash
cargo run -p tiangong-memory-consolidator -- --root ~/.tiangong
```

常用参数：

| 参数 | 说明 | 缺省 |
|---|---|---|
| `--root <目录>` | 天工根目录（存储根）；也可用环境变量 `TIANGONG_STORAGE_ROOT` | `~/.tiangong` |
| `--threshold <0~1>` | 相似召回阈值：达到即送 LLM 判定，越高送审越少 | `0.90` |
| `--batch-size <n>` | 单次 LLM 判定最多包含的记忆数（含种子） | `20` |
| `--max-nodes <n>` | 最多以多少条记忆为种子处理，`0` 表示不限 | `0` |
| `--offset <n>` | 跳过最新的 N 条再开始（断点续跑） | `0` |
| `--dry-run` | 试运行：只报告不落库（仍会调用 LLM 生成预览决策） | 关 |
| `--keep-low-quality` | 保留低质量记忆，不执行遗忘 | 关 |
| `--repair-vectors-only` | 只补写缺失向量后退出，不执行整理 | 关 |
| `--skip-vector-repair` | 整理前跳过缺失向量补写 | 关 |
| `--stage <阶段>` | 只执行指定阶段：`dedup` / `distill` / `similar`，可重复或逗号分隔 | 全部 |
| `--max-llm-calls <n>` | 每个阶段最多调用多少次 LLM，`0` 表示不限 | `0` |
| `--distill-batch-size <n>` | distill：每次 LLM 调用最多包含的工具日志数 | `40` |
| `--distill-window-minutes <n>` | distill：相邻工具日志间隔超过该分钟数即切分时间段 | `30` |
| `--journal <文件>` | 执行记录（JSON Lines）路径 | `<根目录>/data/memory-consolidation/<时间>-<阶段>[-dry-run]-<pid>.jsonl` |

建议第一次先用 `--dry-run` 预览将要发生的合并与遗忘，确认后再正式执行。

## 补写缺失向量

整理前默认先补齐缺向量的活跃记忆（也可用 `--repair-vectors-only` 单独执行）：
工具只读向量表求出缺失差集，用与 Memory 相同的 embedding 模型和节点文本在
**工具侧**计算向量，再经 `upsert_node_vectors` 交给 Memory Leader 写入。
天工运行时无需退出：向量经 IPC 由天工的 Memory 串行写入并立即对召回生效，
不会与 Memory 并发写 LanceDB。Leader 会校验模型与维度、跳过已删除或归档的
记忆；Leader 侧 Memory 版本过旧（不支持该接口）时补写报错退出。

## 执行记录

每次运行（含试运行）都会写一个 JSON Lines 执行记录，启动和结束时打印路径。
每条删除、改写、新建记忆、关联变更以及每次 LLM 判定（完整送审内容与原始输出）
都在**执行前**追加一行并立即落盘（`sync_data`）；被删除的记忆保存完整快照
（全部字段、删除前的关联、证据路径），可据此追溯或人工恢复。记录写入失败即
中断整理——没有记录就不删除。结束行 `run_end` 含完整报告（终端只显示前 200 条
明细）。记录放在 `data/memory` 之外，不受 Memory 迁移/恢复影响。

| event | 内容 |
|---|---|
| `run_start` | 命令行参数、阶段、阈值、模型 |
| `llm_decision` | 送审内容 `prompt` 与 LLM 原始判定 `response` |
| `delete_node` | 被删记忆完整快照 `node`、`relations`、`evidence_path`、`reason`、`merged_into` |
| `update_node` / `create_node` | 改写前 `before`、写入内容 `after`、新建记忆 `id` |
| `upsert_relation` / `delete_relation` | 关联内容 |
| `run_end` | 致命错误、LLM 调用数、完整报告 |

## 三个阶段

整理分三个阶段，缺省按 dedup → distill → similar 顺序全部执行；可用
`--stage`（可重复或逗号分隔）只执行部分阶段。试运行时后面的阶段会跳过前面
阶段「模拟删除」的记忆，统计与真实执行一致。建议分阶段执行：

```bash
# 1. 摘要完全一致的归一（不调用 LLM）
tiangong-memory-consolidator --stage dedup --dry-run
# 2. 工具日志提炼（先小批试跑）
tiangong-memory-consolidator --stage distill --dry-run --max-llm-calls 5
# 3. 相似召回 + LLM 判定
tiangong-memory-consolidator --stage similar --dry-run --max-llm-calls 5
```

- **dedup**：同一范围（工作区）内摘要完全一致（去空白、忽略大小写）的记忆
  直接归一，不调用 LLM。主记忆优先取「决策」类，其次重要性、使用次数、更新
  时间；标题取组内出现次数最多的；重要性取组内最高；组员的关联全部改挂到主
  记忆（重复关联取较大权重）后物理删除组员。摘要短于 20 字的不处理；证据类
  记忆还要求证据路径一致；工具日志留给 distill。
- **distill**：工具日志（标题形如 `fs__file_write 产物`、即 `^[A-Za-z0-9_]+ 产物$`
  的证据类记忆）单条信息量很低。按工作区、按时间段（相邻日志间隔超过
  `--distill-window-minutes`，默认 30 分钟即切段）分批（每批最多
  `--distill-batch-size`，默认 40 条），连同证据文件路径和同时段的经历/决策
  记忆交给 LLM：提炼出新经历（`learned_from` 关联到保留的依据日志，`related_to`
  关联到相关上下文），并判定哪些日志可以删除。**先写新经历再删日志**，被删
  日志的关联改挂到新经历；LLM 没有点名删除的日志一律保留。证据路径经 Memory
  的只读接口 `evidence_paths` 读取，不计入使用次数。
- **similar**：相似召回 + LLM 统一判定（见下节），排除工具日志。

`--max-llm-calls` 对 distill 与 similar 分别计算。只执行 dedup 时不要求配置
LLM，也不读取/补写向量。

## 整理逻辑（similar 阶段）

1. **获取所有记忆**：全库分批拉取活跃记忆——每批最多 5000 条（按更新时间
   从新到旧），整理完自动取下一批，直到某批不足 5000 条即全库整理完成；
   记忆库很大时可用 `--offset` 断点续跑。
2. **相似召回**：读向量库时对全部带向量的记忆做**全量精确余弦**（多线程，
   2 万条约 10 秒），把所有达到 `--threshold` 的相似对都找出来，不截断；
   没有向量库时退化为「经 Memory 召回 + 字符二元组相似度」按阈值过滤。
   **阈值只决定送审范围，不决定是否合并。**
3. **LLM 统一判定**：逐条以未处理记忆为种子，把它与全部尚未一起判定过的相似
   记忆交给 LLM（超过 `--batch-size` 时分多批，每批都含种子）。LLM 在一批中
   自行识别哪些记忆描述同一件事（可给出多个合并组）、哪些需要删除或重新分类、
   需要新建哪些关联；**未被点名的记忆保持原样**。工具会校验 LLM 输出：只接受
   本批 ID、一条记忆至多属于一个合并组、合并组至少两条。LLM 请求失败按间隔
   重试（默认 3 次、间隔 3 秒），重试耗尽即**中断整理并以非零码退出**；
   未配置 LLM 时只合并文本完全相同的记忆。
4. **关联链重建**：被合并记忆的关系先重指向主记忆（保留方向、类型），
   组内互连关系随合并消失；LLM 规划的新关系端点只允许是本批记忆或原关联链上
   出现过的记忆，防止幻觉 ID；同一关联已存在时**取较大权重**，不会把强关联
   覆盖成弱关联。
5. **避免重复送审**：同一对记忆只交给 LLM 判定一次；合并后主记忆继承组员的
   相似记忆，若出现未判定过的新候选则重新排队继续判定，直到全部判定完成；
   `--max-nodes` 限制种子数量并在报告中给出剩余数量。
6. **低质量遗忘**：标题为空、摘要过短且重要性低、长期未用且重要性低的记忆
   会被删除（不再参与召回，也不可恢复）。

合并与遗忘均为**物理删除**：经 Memory 的 `delete_node` 移除记忆行、关联关系、
全文与向量索引，删除后不可恢复；建议先用 `--dry-run` 审阅整理计划。

## 输出示例

```text
天工根目录：/Users/you/.tiangong
记忆 LLM：模型引用 step-mini → step-mini（https://api.stepfun.com/v1）
记忆 Embedding：在线端点 bge-m3（1024 维）
已有 Memory Leader 运行中（pid=1234），本进程经 IPC 转发执行整理
开始记忆深度整理……
整理中 [1/128] Redis 哨兵部署
...
记忆深度整理完成
- 检索后端：vector；扫描活跃记忆：128
- 已处理：96（剩余未处理：0）
- 合并组数：12，合并删除：19，重写归一：12（重分类 5）
- 遗忘低质量记忆：3
- 关联链：重指向重建 7，LLM 新建 2（LLM 调用 12 次）
处理明细：
  - LLM合并 3 条一致记忆为「Redis 哨兵部署」（分类 DomainKnowledge）
  ...
```

## 安全说明

- 整理会修改记忆库，请确认后再执行；`--dry-run` 不改动任何数据。
- 合并与遗忘为物理删除（不可恢复），正式执行前务必审阅 `--dry-run` 的整理计划。
- 低质量判定刻意保守（多重条件同时满足才遗忘），避免误伤短但有用的记忆。
- LLM 的合并结果会做结构校验（`primary_id`、`forget_ids`、关联目标必须在
  给定范围内）；LLM 请求连续失败时中断整理并以非零码退出，不写半成品。
- 与天工同时运行时本进程为 Follower，所有读写经 IPC 转发给 Leader（要求
  Leader 侧 Memory 同样包含 `delete_node` 与 `upsert_node_vectors` 能力）。
