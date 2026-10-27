# DSH Live Translator — 自定义词库格式规范

## 1. 设计目标

词库文件需要同时满足三个互相拉扯的要求：

1. **人可写** —— 用户能用记事本随手加两行术语，不学 YAML。
2. **机可读** —— 程序化生成 / 批量导入时有严格 schema 和版本号。
3. **向后兼容** —— 格式演进时老文件仍能加载。

因此采用「**宽松输入 + 严格内部模型**」策略：6 种输入格式全部归一化到同一个 `GlossaryFile` 内部结构。

---

## 2. 内部数据模型（规范化后的唯一真相）

```rust
pub struct GlossaryFile {
    pub version: u32,          // 缺省视为 1
    pub entries: Vec<Entry>,
}

pub struct Entry {
    pub source: String,        // 必填，源词（英文）
    pub target: String,        // 必填，目标译法（中文）
    pub aliases: Vec<String>,  // 同义触发形式，缺省为空
    pub case_sensitive: bool,  // 缺省 false
    pub scope: Scope,          // Asr | Mt | Both，缺省 Both
    pub group: Option<String>, // 分组
    pub enabled: bool,         // 缺省 true
    pub note: Option<String>,  // 备注，不参与翻译逻辑
    pub regex: Option<String>, // Layer C 兜底正则
    pub source_file: PathBuf,  // 运行时注入，不出现在文件里
    pub line_no: u32,          // 运行时注入，用于错误定位
}

pub enum Scope { Asr, Mt, Both }
```

**归一化规则**（所有格式共用）：

- 文本做 Unicode NFKC 规范化。
- `case_sensitive: false` 时，匹配用 lowercase 折叠后的键，但**还原时用词库里书写的原始大小写**。
- 去除首尾空白；内部连续空白折叠为单空格。
- 空 `source` 或空 `target` 的条目丢弃，并记录警告（不中断加载）。

---

## 3. 输入格式

### 3.1 TSV（推荐首选）

制表符分隔，`#` 开头为注释。第一行若是表头则按表头名映射列；否则按位置读取。

```
# 列序：source / target / aliases / scope / group
Kubernetes	Kubernetes	k8s,K8s	both	技术
i.e.	也就是说		mt	通用
macOS	macOS		both	技术
```

带表头的写法：

```
source	target	scope	note
Kubernetes	Kubernetes	both	官方名称不译
RxJS	RxJS	both	JavaScript 库
```

### 3.2 CSV

标准 CSV（RFC 4180，支持引号转义与字段内逗号），语法与 TSV 一致。

```csv
source,target,aliases,scope,group,enabled,note
"Kubernetes","Kubernetes","k8s;K8s","both","技术","true","官方名称不译"
"e.g.","例如","","mt","通用","true",""
```

> **分隔符注意**：TSV 的 `aliases` 用逗号分隔多值；CSV 里改用 `;` 分隔多值，避免和逗号冲突。

### 3.3 JSON

```json
{
  "version": 1,
  "entries": [
    {
      "source": "Kubernetes",
      "target": "Kubernetes",
      "aliases": ["k8s", "K8s"],
      "scope": "both",
      "group": "技术",
      "enabled": true,
      "note": "官方名称不翻译",
      "regex": null
    }
  ]
}
```

未知字段被忽略（不报错），便于未来扩展字段时旧版仍能读。

### 3.4 YAML

最便于手写的格式，支持注释和分组嵌套：

```yaml
version: 1
groups:
  - name: 技术术语
    entries:
      - source: Kubernetes
        target: Kubernetes
        aliases: [k8s, K8s]
        note: 官方名称不翻译
      - source: React
        target: React
  - name: 产品名
    entries:
      - source: iPhone
        target: iPhone
        scope: both
```

### 3.5 纯文本（最简）

每行 `source` + 制表符或两个以上空格 + `target`。适合「一键批量粘贴」场景。

```
Kubernetes	Kubernetes
i.e.	也就是说
gRPC	gRPC
```

---

## 4. 三层作用机制的精确行为

### 4.1 Layer A — ASR 热词偏置（作用于识别）

- 参与构建热词列表的条目：`scope` 为 `Asr` 或 `Both`，且 `enabled: true`。
- 热词内容 = `source` + `aliases`，去重。
- 词库变更时**只重建热词表**，不重建 recognizer 实例（避免 2~3 秒卡顿）。

### 4.2 Layer B — 翻译前哨兵保护（作用于翻译前）

**只在 `scope` 为 `Mt` 或 `Both` 时生效。**

匹配算法：

1. 对文本做 NFKC 归一化 + 大小写折叠（若 `case_sensitive: false`）。
2. Aho-Corasick 自动机扫描，收集所有命中区间。
3. **冲突消解**：重叠命中取最长匹配；等长时取 `source` 字典序最小的（保证确定性）。
4. 按 `source` 起点位置排序，生成替换序列。
5. 哨兵格式 `▁X{n}X`，`n` 从 0 递增，取当前快照内已用最大索引 +1。
6. 未命中部分原样保留。

还原算法：

1. 在译文上正则查找 `▁X(\d+)X`。
2. 索引越界（理论上不应发生，因为 `SnapshotGuard` 保证快照不变）→ 记录错误并跳过。
3. 替换为该条目的 `target`。
4. **往返校验**：还原后哨兵出现次数应等于替换前命中数。不等则触发 Layer C 并在 UI 标记该段。

选 `▁X{n}X` 的理由：Marian 的 SentencePiece 分词器会把 `▁` 视作词边界前缀，这串字符会被切成独立且未登录的片段，模型不会尝试翻译它，会原样透传。

### 4.3 Layer C — 翻译后强制替换（兜底）

触发条件（三者任一）：

- 哨兵往返校验失败。
- 条目配置了 `regex` 字段。
- 用户在 UI 中手动标记某段「译文需要复核」。

行为：对译文应用该条目的 `regex` 替换（若配置），再做一次 source 的模糊匹配（如编辑距离 ≤ 1 的变形）尝试纠正。

---

## 5. 热重载语义

```
用户保存词库文件
      │
      ▼
notify 触发（250 ms 防抖，连续多次保存合并为一次）
      │
      ▼
构建新 GlossarySnapshot（含 Aho-Corasick 自动机 + 热词表）
      │构建失败（语法错 / 冲突）→ 保留旧快照，UI 显示错误与行号
      ▼
ArcSwap 原子替换
      │
      ├─► 新进入的文本片段：使用新快照
      └─► 已在处理中的片段：继续用 SnapshotGuard 持有的旧快照
```

**SnapshotGuard 是必须的正确性保障**：若处理中途换快照，哨兵索引的语义空间就会错位，还原时会把 A 词的占位符还原成 B 词。

---

## 6. 多文件合并规则

支持同时加载多个词库文件（目录拖入或多次导入）。合并时按归一化后的 `source` 去重：

| 冲突情况 | 处理 |
|---|---|
| 同 `source`，有一条 `enabled: true` | `enabled: true` 的胜出 |
| 同 `source`，多条 `enabled: true` | **后加载的胜出**，UI 高亮该行并提示冲突 |
| 同 `source`，多条 `enabled: false` | 全部跳过 |

`aliases` 在合并时取并集。

---

## 7. 命中统计

每个条目独立计数：

- `asr_hits` —— Layer A 热词在解码中被激活的次数（sherpa-onnx 提供统计接口）。
- `mt_hits` —— Layer B 哨兵替换命中次数。
- `regex_hits` —— Layer C 兜底触发次数。

UI 提供「零命中词条」筛选，帮用户识别失效词条（如拼写错误、实际不会出现的中文词）。

**自增长词库**：导出时把 `mt_hits >= 1` 的条目整理成新词库文件，用户确认后加入正式词库。

---

## 8. 校验与错误处理

| 情况 | 处理 |
|---|---|
| 文件编码非 UTF-8 | 尝试 UTF-8 → GBK → 报错 |
| JSON/YAML 语法错 | 报错并给出行号，保留旧快照 |
| 缺 `source` 或 `target` | 丢弃该条 + 警告，文件其余部分正常加载 |
| `regex` 编译失败 | 该条的 Layer C 禁用，其余功能正常 |
| `aliases` 与 `source` 重复 | 去重 |
| 加载后 Aho-Corasick 自动机内存超限（> 5000 条） | 仍可用，提示可能影响实时性，建议拆分文件 |
