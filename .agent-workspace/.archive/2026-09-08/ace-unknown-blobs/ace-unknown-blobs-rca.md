# RCA · ACE 检索 `400 unknown blobs` 复发(2026-09-08)

> 关联历史修复:`6318402 fix: 跳过空文件, 修检索整体 400 unknown blobs`(2026-07-25)。
> 本次**不是**该修复失效,是同一症状下的**另一条独立成因**。

## 🔴 1. 现象与上下文

- 触发:对 4 个仓库调 `search_context`。`grokx-protocol` / `cockpit-tools` 正常;
  `F:\cursor-account-toolkit\cursor-reg` 报 `400 Bad Request - unknown blobs: <21 个 hash>`;
  `E:\ecom-copilot` 只回 `SECURITY.md`(score 0.216,全量源码不可检索)。
- 报错的 21 个 hash **一一对应该项目被索引的全部 21 个文件**(`register.py` / `README.md` /
  `outlook_mail.py` / `tests/*.py` …),且这些文件**没有一个是空文件**(实测全部 > 0 字节)。
  → 与 6318402 的"空文件"根因不同。

**成功态(§0.15B · 来源 = 用户原话「重新调用 mcp 测试俩个仓库…以确认是否恢复」)**:
NOT「客户端日志打印 indexing successfully」,BUT「用户对任意仓库调 ACE 检索,拿到本项目
真实源码片段」。负条件:不得出现 `400 unknown blobs`,也不得只回单个无关文件。

**现象锁定(单一可证伪命题)**:
对 `cursor-reg` 调 `search_context`,服务端 `/agents/codebase-retrieval` 返回 400,
错误体列出的未知 blob 数 = 21 = 本地 `.ace-tool/index.bin` 记录的 blob 总数,
即**服务端一个都不认识**(不是部分缺失)。

## 🔍 1.5 假设台账

| ID | 假设 | 状态 | 证伪/确认证据 | 更新 |
|----|------|------|------|------|
| A | 6318402「空文件」根因复发 | 🔴falsified | 枚举 cursor-reg 全部文件,最小 64 字节,无 0 字节文件;报错 hash 对应的是 `register.py`(19739B)等正常文件 | 09-08 21:39 |
| B | 部署的二进制版本旧于修复 | 🔴falsified | `mcp_settings.json` 用 `@7iook/ace-tool-rs@0.1.17`;`git rev-list -n1 v0.1.17` = `a81180f` = HEAD,含 6318402 + 22f6861;npx 缓存 `package.json` 实测 `"version": "0.1.17"` | 09-08 21:38 |
| C | 服务端故障/blob 库损坏,客户端无解 | 🔴falsified | 删 `index.bin` 后 `--index-only` 全量重传,3 批共回 21 个 `blob_names`、`skipped_blobs: []`,随后检索 rank1 命中 `register.py`(0.586)。服务端功能完好 | 09-08 21:43 |
| D | 两个后端(finnian / jikeai)共用同一份 index.bin 导致错配 | 🟡部分成立(是**放大器**不是本次触发源) | `calculate_config_hash()` 只含 `max_lines_per_blob`,不含 base_url;对照实验:同一批 hash + 同一 `X-Ace-Project`,finnian 回 200 有内容、jikeai 回 200 空结果 → 索引确实跨后端共用,但 jikeai 不返 400,非本次 400 来源 | 09-08 21:46 |
| E | **本地索引与服务端状态失同步后,客户端永不自修** | 🟢confirmed | 见 §2,直接实测 | 09-08 21:40 |

## 🔍 2. 根因分析

**First Broken Point**:`src/index/manager.rs:1277`(Step 6 `save_index`)——
索引落盘**不与服务端确认结果对账**。

三个互相咬合的缺陷,构成一个**无法自愈的吸收态**:

1. **上传结果不回填索引**(`manager.rs:1220-1280`)
   `new_index` 在 Step 4 用**本地算出的 hash** 构建完毕;Step 5 上传;Step 6 无条件
   `save_index(&new_index)`。`uploaded_blob_names` 只参与统计计数,**从不用于过滤
   `new_index`**。批次失败时 `upload_blobs_adaptive` 只 `failed_batch_count += 1`
   (`manager.rs:817-822`),失败批次里的 blob 照样以"已索引"身份写进 `index.bin`。

2. **mtime 缓存把谎言永久化**(`manager.rs:1246-1252`)
   下一轮 `process_file_standalone` 见 mtime 未变 → 判 `Cached` → `new_blobs` 为空 →
   日志打 `No new files to upload, using cached index` → **永不重传**。
   实测(21:40,当时的卡死态):
   ```
   Found 21 files to process
   Incremental indexing: 21 cached blobs, 0 new blobs
   No new files to upload, using cached index
   Indexing completed successfully: Indexed 21 blobs (cached: 21, new: 0)
   ```
   客户端报告"成功",而服务端一个 blob 都没有。

3. **检索失败不触发失效**(`manager.rs:1451`)
   `search_context` 收到非 2xx 直接 `return Err(...)`,既不解析 400 里的未知 hash 列表,
   也不剔除 `index.bin` 对应条目、更不重传重试。

**闭环**:任何一次"服务端最终没拿到 blob"(批次失败 / 服务端 GC 或保留期到期 /
换后端 / 换租户),都会让本地索引与服务端**永久失同步**;此后每一次检索都 400,
且**没有任何路径能自动恢复**——只有人工删 `index.bin`。

**Bug class**(§5.2 七分类):**Cache/State Pollution**
(次要伴生:Interface Contract Ambiguity —— `blob_names` 是服务端权威回执,客户端未消费)。

**本次为什么"看起来是修复失效"**:
`cursor-reg` 的 `index.bin` 建于 2026-08-16,报错发生在 09-08,相隔 3 周。
中途服务端侧这批 blob 不再存在(GC/保留期/迁移,外部不可观测),而客户端因缺陷 2
坚信"已上传"。用户上一轮尝试删除时,`Invoke-SafeClean` 因该仓库**无 git remote**
拒绝执行(C-010 保护),`.ace-tool` 实际**没被删掉**(21:15 的 `CACHEDIR.TAG` 仍在),
所以症状原样保留 —— 这解释了"为什么 ecom-copilot 删了就好、cursor-reg 删了却没好"。

## 🕵️ 3. 变体扫描(§5.3)

**重复实现审查**:客户端内是否已有"服务端回执对账 / 索引失效"机制?
`fast-context`(query: `unknown blobs error handling, blob upload to server, index.bin
local index file, sync blobs before search`)+ `Select-String config_hash` 全仓核对 →
**verified absent**:全仓仅 `calculate_config_hash` 一处失效判据,且只覆盖
`max_lines_per_blob`;无任何 blob 回执对账、无检索失败后的索引失效路径。故须新建,非重复造轮。

**指纹**:「本地缓存单方面宣称远端状态,且无对账、无失效、无自愈」。

| 位置 | 风险 | 本轮修? | 说明 |
|---|---|---|---|
| `manager.rs:1277` save_index 不对账 | P0 | 待定 | 主根因 |
| `manager.rs:817-822` 失败批次静默计数 | P0 | 待定 | 失败 blob 仍入索引 |
| `manager.rs:1451` 检索 400 不失效索引 | P0 | 待定 | 唯一能覆盖全部触发源的自愈点 |
| `manager.rs:166` config_hash 不含 base_url | P1 | 待定 | 双后端共用索引(假设 D),换后端必错配 |
| 全机 90 份 `.ace-tool/index.bin` | P1 | 否 | 存量潜伏面,见下 |

**存量暴露面**:`everything-search` 实测本机 **90 份** `index.bin`,最早建于 2026-08-13。
每一份建立时间早于服务端 blob 保留期的,都是同一颗定时炸弹,且症状各异
(finnian 报 400 / jikeai 静默回空 → 表现为"检索质量突然变差",比 400 更难发现)。

## 👥 4. 真实场景压力模拟(§5.4)

1. **服务端 blob 保留期到期 / GC**(本次实际命中)——索引宣称已传,实际已无。
   现状:永久 400。修复后:检索 400 → 解析未知 hash → 剔除 → 重传 → 重试一次 → 恢复。
2. **上传中途断网,部分批次失败** —— 现状:失败批次的 blob 仍写入索引,status 虽为
   `partial` 但索引已被污染,下轮 mtime 命中不再重传 → 永久 400。
   修复后:只回填服务端确认过的 hash,未确认的下轮自然重传。
3. **同一项目在两个 ACE 后端之间切换**(mcp_settings 实配 finnian + jikeai)——
   现状:index.bin 不区分后端,切过去必然全量 unknown;finnian 报 400,
   jikeai 静默降级回空结果(**更危险:无报错的检索质量塌陷**)。
   修复后:base_url 进 config_hash,切后端自动全量重建。
4. **多进程并发**(MCP 常驻进程 + `--index-only` CLI 同时跑)——`save_index` 已是原子写
   (tmp + rename),但两者可能基于不同快照互相覆盖。本轮不处理,登记为已知边界。

**本轮明确不处理**:服务端 400 的整体拒绝语义(外部服务,无源码,不可改);
多进程并发覆盖(需要文件锁,超出本次根因半径)。

## 📚 5. 行业参照

- `exa.web_search_exa`,query:`client side content-addressable blob cache assumes server
  still has blob, stale local manifest never re-uploads, self-heal on unknown blob error`
- top1:**mdcast-client (docs.rs)** — https://docs.rs/mdcast-client/latest/mdcast_client/
  同类内容寻址 blob 协议的标准解法,原文:*"posts the render, and — if the server answers
  `409` with blobs it does not hold — uploads exactly those and retries once."*
  并提供 `Client::preflight`(`POST /v1/blobs/check` 预检)作为悲观路径。
  **结论**:「乐观提交 + 按服务端回执补传 + 重试一次」是该问题域的既定范式,
  我们缺的正是"按回执补传"这一环。
- 佐证 2:`@hdae/fetch-cache` (JSR) — https://jsr.io/@hdae/fetch-cache
  明确把 *"corrupted cache entries are evicted and re-fetched from the source of truth
  (fail loud)"* 列为一等特性 —— **缓存必须能自我失效**,与本 RCA 结论同向。

## 🛠️ 6. 外科手术式修复(建议方案,待批准)

**修复层**:全部在客户端(`ace-tool-rs`),**服务端零改动**。

- **F1 · 回执对账**(`manager.rs` Step 5/6 之间):把 `uploaded_blob_names` 收成
  `HashSet`,落盘前从 `new_index` 剔除**未被服务端确认**的 blob hash。
  → 失败批次 / 被服务端跳过的 blob 不再以"已索引"身份入库,下轮自动重传。
- **F2 · 检索自愈**(`manager.rs:1451` 非 2xx 分支):识别 `400` + `unknown blobs`,
  解析出 hash 列表 → 从 `index.bin` 剔除命中条目 → 重新 `index_project()`(强制重传)
  → **重试一次**。这是唯一能覆盖**全部触发源**(GC / 换后端 / 部分失败)的兜底。
- **F3 · 索引按后端隔离**(`manager.rs:166`):`calculate_config_hash` 纳入 `base_url`
  (以及 token 指纹,避免同域不同租户)。→ 换后端自动判定 config mismatch 并全量重建。

**刻意不改的地方**:不在 `search_context` 加 `if err return Ok(空)` 之类吞错兜底;
不为绕开 400 而在检索前做全量 blob 预检(多一次全量 RTT,mdcast 也仅将其列为可选悲观路径)。

**验收**:F1/F2 需 TDD 红→绿 —— 先写「服务端只确认部分 blob → 索引不得记录未确认 hash」
与「检索首次 400 unknown blobs → 自动重传并二次成功」两个失败用例。

## ⚠️ 7. 影响面与回归风险

- 影响面:`IndexManager::index_project` / `search_context` / `calculate_config_hash`,
  即所有 ACE 检索调用方(全机 90 个已索引项目)。
- F3 上线后**首次调用会全量重建索引**(一次性上传开销),需在发布说明写明。
- **消费锚点(§0.15B)**:最终 sink = MCP `search_context` 返回给 Agent 的检索结果。
  真实 e2e 已跑通一次(非单测):`--index-only` 全量重传 → MCP `search_context`
  → rank1 `register.py` score 0.586。
- 回归护栏:`tests/` 内补 F1/F2 用例;`cargo fmt --check` + `clippy -D warnings`。

## 🧩 8. 边界加固

本次顺带暴露并已登记的边界缺口:
① 上传回执 `blob_names` 是服务端权威状态,此前**完全未被消费**(仅用于计数);
② 本地索引缺少"归属哪个后端/租户"的身份维度;
③ 检索链路缺少失败反馈到索引层的回边。
三者同源:**本地缓存被当作真相,而非当作对远端真相的一份可能过期的断言**。

## 任务清单

- [x] 定位根因并证伪历史修复复发假设 — **Evidence**: verify=`--index-only` 实测日志
  (21:40 卡死态 / 21:41 重传态);files=`src/index/manager.rs`;AC=台账 E 行 🟢;commit=`n/a(诊断)`
- [x] 确认服务端功能完好、问题在客户端 — **Evidence**: verify=3 批 upload 回 21 个
  `blob_names`+检索 rank1 命中;AC=台账 C 行 🔴;commit=`n/a(诊断)`
- [x] 恢复 cursor-reg / ecom-copilot 可用性 — **Evidence**: verify=两仓 `search_context`
  均返回本项目真实源码;commit=`n/a(仅删本地 index.bin)`
- [x] F1 回执对账(含 TDD 红→绿) — **Evidence**: verify=`cargo test --test backend_resync_test` 7 passed;
  红灯证据=撤回实现后 `server_confirming_only_some_uploaded_blobs_keeps_the_rest_out_of_the_saved_index`
  panic `unconfirmed file must NOT be recorded as indexed`;files=`src/index/manager.rs`(Step 5.5);
  AC=未确认 blob 不入索引且下轮重传;commit=`pending`
- [x] F2 检索 400 自愈重试(含 TDD 红→绿) — **Evidence**: verify=真实 e2e(见 Update Log 2026-09-08 22:50);
  files=`src/index/manager.rs`(`SearchError` / `parse_unknown_blob_hashes` / `invalidate_blobs` /
  `search_context_once`);AC=首次 400 后自动重传并二次成功,且恰好只重试一次;commit=`pending`
- [x] F3 索引按后端隔离 — **Evidence**: verify=真实双后端 e2e,RUN3 切回 `2 cached, 0 new`;
  files=`src/utils/project_detector.rs`;AC=每后端独立索引文件 + 切回缓存命中;commit=`pending`
- [x] F1/F2 多 chunk 文件 all-or-nothing(主 AI 复核时发现的变体) — **Evidence**:
  verify=撤回后 2 用例红(`left: 0, right: 3`),恢复后 7 passed;AC=任一 chunk 未确认则整条 entry 丢弃;commit=`pending`
- [x] 存量 90 份 index.bin 的处置 — **Evidence**: F3 改名后旧文件自然不再被读取,
  客户端 `info!` 提示可手动删除;不新增删除逻辑(避免在客户端引入破坏性路径);commit=`pending`

## Update Log

- 2026-09-08 21:50 · 诊断完成。结论:**客户端可解,服务端无需改动**。
  历史修复 6318402 未失效,本次是同症状下的另一条独立成因(缓存状态污染)。
  已通过删除 `index.bin` 恢复两个仓库;F1/F2/F3 待批准后实施。

- 2026-09-08 22:50 · F1/F2/F3 实施完成(executor 主体实现 + 主 AI 复核补漏)。

  **F3 的形态相对 §6 方案做了升级**:原方案是把 `base_url` 塞进 `calculate_config_hash`,
  那样换后端只是"自动失效重建",A→B→A 要全量传两次。改为**索引文件按后端分文件**
  (`.ace-tool/index-<fp>.bin`,`fp = sha256("v1:"+base_url+"\0"+token)[..8]`),改动量几乎相同,
  但 A→B→A 切回是缓存命中。`calculate_config_hash` 维持原样(它管的是 `max_lines_per_blob`
  这个正交维度,不合并)。遗留 `index.bin` 不读不删,仅 `info!` 提示。

  **主 AI 复核时发现并修掉的变体(executor 原实现的真实缺口)**:
  F1 对账与 F2 失效原本都用 `retain` **保留已确认的 hash 子集**。但 `FileEntry.blob_hashes` 是 `Vec`,
  超过 `max_lines_per_blob` 的文件会切成多 chunk,可能跨批次。一旦某 chunk 落在失败批次里,
  这条 entry 会带着半份 hash 存活 → 下轮 mtime 命中缓存 → **缺失 chunk 永不重传**,
  且因为留下的 hash 服务端都认识,**连 400 都不会报**,退化成静默的半索引文件。
  已改为 entry 级 all-or-nothing(任一 hash 未确认/未知 → 整条丢弃 → 整文件重新处理)。
  这与主根因同源:局部真相比没有真相更危险,因为它骗过了缓存。
  红灯证据:撤回该修复后两个新用例失败,`left: 0, right: 3` —— 修复过程一个 chunk 都没重传。

  **真实 e2e(非单测;真实后端 finnian + jikeai,真实 MCP stdio 协议)**:
  - F3:同一项目 RUN1(A) `0 cached / 2 new` → RUN2(B) `0 cached / 2 new` 且生成第二份索引文件
    → RUN3(切回 A) `2 cached / 0 new`。两份文件 `index-e008451474ec9c26.bin` / `index-fe59fbc4aaeb52cd.bin`。
  - F2:构造"只上传到 B 的独有内容 + 把索引改名成 A 的指纹",对 A 发一次真实检索。日志链路:
    `1 cached, 0 new` → `Search rejected 1 unknown blob hash(es) ... retrying once: 400 Bad Request`
    → `Invalidated 1 unknown blob hash(es), dropping 1 stale index entry` → `0 cached, 1 new`
    → `Uploading 1 new chunks` → `Search complete`,结果命中 marker,score 1.000。
    这正是改之前永久失败、只能人工删文件的场景。

  **顺带**:`tests/tiering_test.rs` 有 4 处**改动前就存在**的格式债务,而 CI 跑
  `cargo fmt --all -- --check`,即 CI 的 fmt 闸门在 HEAD 上本来就是红的。
  已单独一个 `style:` 提交修掉,不与本次 fix 的 diff 混在一起。

  验收:`cargo fmt --check` 0 差异 · `cargo clippy --all-targets --all-features -- -D warnings`
  零警告 · `cargo test` 全绿(新增 `tests/backend_resync_test.rs` 7 个用例:F1×2 / F2×4 / F3×1)。
