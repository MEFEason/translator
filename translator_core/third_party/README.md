# 第三方源码（submodule）

## bergamot-translator

**用途**：离线翻译引擎（Marian NMT）。Mozilla 出品，即 Firefox
Translations 的底层引擎。

**为什么用它而不是 CTranslate2**：

| | CTranslate2 | **Bergamot（本项目选用）** |
|---|---|---|
| 构建方式 | `ctranslate2-sys` **联网下载**预编译包 | 纯 CMake 源码编译 |
| 离线构建 | ❌ 实测代理 502 即失败 | ✅ 仅需 C++17 |
| 分词器 | 收 token 级输入，需自接 SPM | **收原始文本**，内置 SPM |

两者同为 Marian 框架，**权重完全复用**，翻译质量无差异。

**许可**：MPL-2.0

### 获取源码

```bash
git submodule update --init --recursive
```

`--recursive` 必须加 —— Bergamot 自身还有 marian、sentencepiece、
dllama 等子模块。

### 权重

Bergamot 权重与 Marian 不同源，不能直接用 `opus-mt-en-zh` 的 PyTorch
权重。需要从 bergamot 模型仓库获取（Mozilla 的模型分发地址）。
权重**不入库**，由用户在应用内导入 `.dshm` 包。
