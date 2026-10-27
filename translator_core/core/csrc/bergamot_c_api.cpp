// Bergamot 的 C ABI 包装层。
//
// ## 为什么需要这一层
//
// Bergamot 只提供 C++ 接口（`marian::bergamot::AsyncService`），
// Rust 无法直接 FFI 调用 C++ 虚表/模板。本文件把用到的四个操作
// 包成稳定的 C ABI，Rust 侧用 `extern "C"` 声明即可。
//
// ## 生命周期约定（Rust 侧必须遵守）
//
// - `bergamot_translator_create` 返回不透明句柄，失败返回 NULL
// - `bergamot_translator_translate*` 写出的每个 `char*` 都由
//   `new std::string(s).c_str()` 分配，**必须**用
//   `bergamot_translator_free_string` 释放（Rust 侧用 CString::from_raw）
// - 句柄非线程安全的重入：Rust 侧用 Mutex 包住整个翻译调用
//
// ## 模型加载
//
// Bergamot 的权重不是"单文件 + 目录"结构，而是一份 **yml 配置**
// （列出 models / vocabs / shortlist）。配置里可以用相对路径，
// 相对路径以**配置文件自身所在目录**为基准解析
// （见 marian parser.cpp 的 `pathsInSameDirAs` 参数）。
// 所以调用方只需把配置文件绝对路径传进来，无需关心工作目录。

#include <cstdlib>
#include <cstring>
#include <future>
#include <memory>
#include <string>
#include <vector>

#include "translator/parser.h"
#include "translator/response.h"
#include "translator/response_options.h"
#include "translator/service.h"

// 注意：**不能**写 `using namespace marian::bergamot;`
//
// marian 全局命名空间有 `typedef std::shared_ptr<spdlog::logger> Logger;`
// （marian-dev/src/common/logging.h:140），而 `marian::bergamot` 里有个
// 同名的 `class Logger`（src/translator/logging.h:8）。
// 一旦在全局作用域 using 整个命名空间，marian 自己的头文件里
// `Logger log = spdlog::get(logger);` 就变成二义 → C2872。
//
// 所以这里一律用显式限定名。
using marian::bergamot::AsyncService;
using marian::bergamot::Response;
using marian::bergamot::ResponseOptions;
using marian::bergamot::TranslationModel;
using marian::bergamot::parseOptionsFromFilePath;

namespace {

/// 不透明句柄的内部实现。
struct TranslatorHandle {
  AsyncService service;
  std::shared_ptr<TranslationModel> model;
  ResponseOptions responseOptions;

  explicit TranslatorHandle(const AsyncService::Config &cfg) : service(cfg) {}
};

/// 把 std::string 复制成调用方可用 `free` 释放的 C 字符串。
///
/// 刻意用 malloc + memcpy 而不是 `new std::string(...).c_str()`：
/// 后者返回的是 std::string **内部缓冲区**的指针，与 string 对象本身
/// 的生命周期绑定，用 `delete`/`delete[]` 释放它是未定义行为。
/// 标准 C ABI 的做法是成对 malloc/free。
char *dupToCString(const std::string &s) {
  char *buf = static_cast<char *>(std::malloc(s.size() + 1));
  if (buf == nullptr) {
    return nullptr;
  }
  std::memcpy(buf, s.c_str(), s.size());
  buf[s.size()] = '\0';
  return buf;
}

/// 把 C 字符串数组翻译成 C 字符串数组（每个元素由调用方 free）。
std::vector<char *> translateBatch(TranslatorHandle *h,
                                   const std::vector<std::string> &sources) {
  std::vector<char *> out(sources.size(), nullptr);
  for (size_t i = 0; i < sources.size(); ++i) {
    // 阻塞等待：AsyncService 的回调是异步的，这里用 promise 转同步。
    std::promise<Response> promise;
    std::future<Response> future = promise.get_future();
    auto callback = [&promise](Response &&response) {
      promise.set_value(std::move(response));
    };

    h->service.translate(h->model, std::string(sources[i]), callback,
                         h->responseOptions);

    Response response = future.get();
    out[i] = dupToCString(response.target.text);
  }
  return out;
}

}  // namespace

extern "C" {

/// 创建翻译器。
///
/// @param config_path  Bergamot yml 配置文件的**绝对路径**
/// @param num_threads  工作线程数（<=0 时用 1）
/// @param verbosity    日志级别（0=只报错，1=info，2=debug）
/// @return 句柄；失败返回 NULL
void *bergamot_translator_create(const char *config_path, int num_threads,
                                 int verbosity) {
  if (config_path == nullptr) {
    return nullptr;
  }
  try {
    AsyncService::Config serviceConfig;
    serviceConfig.numWorkers = num_threads > 0 ? (size_t)num_threads : 1;

    // 日志级别：Bergamot 的 Logger::Config 类型名与 spdlog 的 Logger 冲突
    // （`Logger::Config` 会被解析成 spdlog::logger），且各版本字段有变动。
    // 保守起见沿用默认配置，详细日志由 Rust 侧 log 层承接。

    // 解析配置（相对路径按配置文件所在目录解析）
    auto options = parseOptionsFromFilePath(std::string(config_path));
    if (!options) {
      return nullptr;
    }

    auto handle = new TranslatorHandle(serviceConfig);
    handle->model = handle->service.createCompatibleModel(options);
    if (!handle->model) {
      delete handle;
      return nullptr;
    }
    return static_cast<void *>(handle);
  } catch (const std::exception &) {
    // 跨 C ABI 边界绝不能抛异常 —— 统一吞掉并返回 NULL。
    // 详细原因由 Rust 侧根据"返回 NULL"给出可读错误。
    return nullptr;
  } catch (...) {
    return nullptr;
  }
}

/// 销毁翻译器。传 NULL 是合法的（no-op）。
void bergamot_translator_destroy(void *handle) {
  if (handle == nullptr) {
    return;
  }
  auto *h = static_cast<TranslatorHandle *>(handle);
  h->model.reset();
  delete h;
}

/// 翻译单条文本。
///
/// @param out 写出的 `char*` 由调用方用 `bergamot_translator_free_string` 释放
void bergamot_translator_translate(void *handle, const char *source,
                                    char **out) {
  if (out == nullptr) {
    return;
  }
  *out = nullptr;
  if (handle == nullptr || source == nullptr) {
    return;
  }
  try {
    auto *h = static_cast<TranslatorHandle *>(handle);
    auto results = translateBatch(h, {std::string(source)});
    *out = results.empty() ? nullptr : results[0];
  } catch (...) {
    *out = nullptr;
  }
}

/// 批量翻译。
///
/// @param source_count 源文本条数
/// @param sources      源文本数组（每项为 C 字符串）
/// @param out          写出的 `char*` 数组，长度与 source_count 相同，
///                      每项由调用方用 `bergamot_translator_free_string` 释放
void bergamot_translator_translate_multi(void *handle, int source_count,
                                          const char *const *sources,
                                          char **out) {
  if (out == nullptr) {
    return;
  }
  for (int i = 0; i < source_count; ++i) {
    out[i] = nullptr;
  }
  if (handle == nullptr || sources == nullptr || source_count <= 0) {
    return;
  }
  try {
    auto *h = static_cast<TranslatorHandle *>(handle);
    std::vector<std::string> inputs;
    inputs.reserve((size_t)source_count);
    for (int i = 0; i < source_count; ++i) {
      inputs.emplace_back(sources[i] ? sources[i] : "");
    }
    auto results = translateBatch(h, inputs);
    for (int i = 0; i < source_count && i < (int)results.size(); ++i) {
      out[i] = results[i];
    }
  } catch (...) {
    // 失败时全部保持 nullptr，Rust 侧按空结果处理
  }
}

/// 释放 `bergamot_translator_translate*` 写出的字符串。
///
/// 与 `dupToCString` 的 malloc 配对。
void bergamot_translator_free_string(char *s) {
  if (s == nullptr) {
    return;
  }
  std::free(s);
}

}  // extern "C"
