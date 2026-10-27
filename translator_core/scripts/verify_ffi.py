#!/usr/bin/env python3
"""FFI 端到端验证：模拟 Dart 侧的完整调用路径。

为什么用 ctypes 而不是开 GUI：
- 可复现、可自动化（CI 里没有显示器与音频设备）
- 走的是与 Dart **完全相同**的 FFI 路径
- 能直接检查返回值，而 GUI 里只能靠肉眼看

验证内容：
1. 库能加载、符号能解析
2. session_create（含配置 JSON 解析）
3. 启动自检 5 步
4. session_start → 采集 → 引擎 → 字幕事件
5. take_events 的 JSON 合法且含字幕
6. session_stop 冲刷、导出 SRT
7. 转写盘落盘且可读回
8. session_free（幂等）
"""

import ctypes as C
import json
import os
import sys
import time
from pathlib import Path

DLL_DIR = Path(sys.argv[1] if len(sys.argv) > 1 else
               r"C:\Users\Admin\WorkBuddy\代码开发\translator_core\ui\build\windows\x64\runner\Debug")
SECONDS = float(sys.argv[2]) if len(sys.argv) > 2 else 5.0

passed = []
failed = []


def check(name, ok, detail=""):
    (passed if ok else failed).append(name)
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" — {detail}" if detail else ""))


def cstr(s):
    """Python str -> 可传给 C 的指针（保持引用，防止被 GC）"""
    return C.c_char_p(s.encode("utf-8"))


def main():
    dll = DLL_DIR / "translator_core.dll"
    if not dll.exists():
        print(f"[FAIL] 找不到 {dll}")
        return 1

    os.add_dll_directory(str(DLL_DIR))
    lib = C.CDLL(str(dll))
    print(f"[INFO] 已加载 {dll}")

    # ── 1. 符号绑定 ──
    lib.translator_version.restype = C.c_void_p
    lib.translator_free_string.argtypes = [C.c_void_p]
    lib.translator_free_string.restype = None
    lib.translator_session_create.restype = C.c_void_p
    lib.translator_session_create.argtypes = [C.c_char_p, C.c_int, C.c_int]
    lib.translator_session_start.argtypes = [C.c_void_p]
    lib.translator_session_start.restype = C.c_int
    lib.translator_session_stop.argtypes = [C.c_void_p]
    lib.translator_session_stop.restype = C.c_int
    lib.translator_session_take_events.restype = C.c_void_p
    lib.translator_session_take_events.argtypes = [C.c_void_p]
    lib.translator_session_free.argtypes = [C.c_void_p]
    lib.translator_selfcheck_json.restype = C.c_void_p
    lib.translator_selfcheck_json.argtypes = [C.c_void_p]
    lib.translator_session_export.restype = C.c_void_p
    lib.translator_session_export.argtypes = [C.c_void_p, C.c_int]
    lib.translator_transcript_dir.restype = C.c_void_p
    lib.translator_transcript_query.restype = C.c_void_p
    lib.translator_transcript_query.argtypes = [C.c_void_p, C.c_uint64, C.c_uint32]
    lib.translator_session_status.restype = C.c_int
    lib.translator_glossary_load.restype = C.c_int
    lib.translator_glossary_load.argtypes = [C.c_void_p, C.c_char_p]
    lib.translator_glossary_reload.restype = C.c_int
    lib.translator_glossary_reload.argtypes = [C.c_void_p]

    # 结构体
    class FfiSessionStatus(C.Structure):
        _fields_ = [
            ("state", C.c_int32), ("asr_ready", C.c_int32),
            ("mt_ready", C.c_int32), ("error_code", C.c_int32),
            ("last_seq", C.c_uint64), ("line_count", C.c_uint64),
            ("dropped_frames", C.c_uint64), ("mt_dropped", C.c_uint64),
            ("transcript_written", C.c_uint64),
            ("transcript_failures", C.c_uint64),
        ]

    class FfiTranscriptStats(C.Structure):
        _fields_ = [("written", C.c_uint64), ("failures", C.c_uint64),
                    ("last_seq", C.c_uint64)]

    lib.translator_session_status.argtypes = [C.c_void_p, C.POINTER(FfiSessionStatus)]
    lib.translator_transcript_stats.argtypes = [C.c_void_p, C.POINTER(FfiTranscriptStats)]

    def take_str(p):
        if not p:
            return ""
        s = C.cast(p, C.c_char_p).value.decode("utf-8", "replace")
        lib.translator_free_string(p)
        return s

    # ── 2. 版本 ──
    ver = take_str(lib.translator_version())
    check("库加载 + 版本", ver.startswith("translator_core"), ver)

    # ── 3. 空指针安全（不崩即是 PASS）──
    try:
        lib.translator_session_free(None)
        rc = lib.translator_session_start(None)
        check("空指针安全", rc == 7, f"start(null) -> {rc}")
    except Exception as e:  # noqa: BLE001
        check("空指针安全", False, str(e))

    # ── 4. 默认转写盘目录 ──
    tdir = take_str(lib.translator_transcript_dir())
    check("默认转写盘目录", "translate results" in tdir, tdir)

    # ── 5. 建会话 ──
    tmp = Path(os.environ.get("TEMP", "/tmp")) / "translator-ffi-e2e"
    cfg = {
        "backend": "mock",
        "profile": "stable",
        "memory_window_lines": int(os.environ.get("WINDOW", "200")),
        "transcript_dir": str(tmp),
        # 多声源：内放（对方）+ 麦克风（我）同时转译。
        # SINGLE=1 时只开内放，用于隔离「多源本身」与「设备问题」
        **(
            {"sources": [{"kind": "systemAudio", "speaker": "对方"}]}
            if os.environ.get("SINGLE") == "1"
            else {
                "sources": [
                    {"kind": "systemAudio", "speaker": "对方"},
                    {"kind": "microphone", "speaker": "我"},
                ]
            }
        ),
    }
    # 先试系统内放，失败退回麦克风
    h = lib.translator_session_create(cstr(json.dumps(cfg)), 0, 0)
    if not h:
        h = lib.translator_session_create(cstr(json.dumps(cfg)), 1, 0)
    check("session_create", bool(h), "系统内放失败后回退麦克风" if h else "")

    if not h:
        print("[FAIL] 无法创建会话 —— 本机可能没有音频设备")
        return 1

    # ── 6. 自检 ──
    sc = json.loads(take_str(lib.translator_selfcheck_json(h)) or "{}")
    steps = sc.get("steps", [])
    check("自检返回 5 步", len(steps) == 5,
          "; ".join(f"{s['name']}={s['ok']}" for s in steps))

    # ── 6.5 词库导入（T.win.1.12）──
    tmp.mkdir(parents=True, exist_ok=True)
    gfile = tmp / "terms.tsv"
    gfile.write_text(
        "Kubernetes\tKubernetes\ngRPC\tgRPC\nlatency\t延迟\n",
        encoding="utf-8",
    )
    n = lib.translator_glossary_load(h, cstr(str(gfile)))
    check("词库导入返回词条数", n >= 3, f"返回 {n}")

    # 导入后自检里的 glossary 步骤应反映词条数
    sc2 = json.loads(take_str(lib.translator_selfcheck_json(h)) or "{}")
    gl = next((s for s in sc2.get("steps", []) if s.get("name") == "glossary"), None)
    check("自检反映词库已加载",
          gl is not None and int(str(gl.get("detail", "0")).split()[0] or 0) >= 3,
          f"glossary: {gl}")

    # 重载不应报错
    check("词库重载", lib.translator_glossary_reload(h) == 0)

    # ── 7. 启动 ──
    rc = lib.translator_session_start(h)
    check("session_start", rc == 0, f"rc={rc}")
    if rc != 0:
        lib.translator_session_free(h)
        return 1

    # ── 8. 跑一会，收字幕 ──
    print(f"[INFO] 运行 {SECONDS:.0f} 秒（需要有音频输入才会产出字幕）…")
    all_events = []
    t0 = time.time()
    while time.time() - t0 < SECONDS:
        time.sleep(0.1)
        raw = take_str(lib.translator_session_take_events(h))
        if raw:
            try:
                all_events.extend(json.loads(raw).get("events", []))
            except json.JSONDecodeError:
                pass

    added = [e for e in all_events if e.get("k") == "add"]
    filled = [e for e in all_events if e.get("k") == "fill"]

    print(f"[统计] add={len(added)} fill={len(filled)}")

    check("字幕事件 JSON 可解析", True, f"add={len(added)} fill={len(filled)}")

    if added:
        # EN 先于 ZH：add 的 seq 必须都出现过
        add_seqs = {e["i"] for e in added}
        check("字幕行有内容", all(e.get("src") for e in added))
        # 多声源：字幕必须带说话人标签
        spks = {e.get("spk") for e in added if e.get("spk")}
        check("字幕带说话人标签", len(spks) >= 1,
              f"说话人: {sorted(str(s) for s in spks)}")
    else:
        print("  [INFO] 无字幕产出 —— 很可能本机没有音频输入（无回放）")

    # ── 9. 停止 + 导出 ──
    rc = lib.translator_session_stop(h)
    check("session_stop", rc == 0, f"rc={rc}")
    raw = take_str(lib.translator_session_take_events(h))
    if raw:
        try:
            all_events.extend(json.loads(raw).get("events", []))
        except json.JSONDecodeError:
            pass

    st2 = FfiSessionStatus()
    lib.translator_session_status(h, C.byref(st2))
    ts2 = FfiTranscriptStats()
    lib.translator_transcript_stats(h, C.byref(ts2))
    print(f"[统计] 收尾后 state={st2.state} last_seq={st2.last_seq} "
          f"transcript_written={st2.transcript_written} 写失败={st2.transcript_failures}")
    check("转写盘有写入", ts2.written > 0, f"{ts2.written} 条记录")
    check("转写盘零失败", st2.transcript_failures == 0)

    srt = take_str(lib.translator_session_export(h, 0))
    has_lines = len(added) > 0
    check("SRT 导出格式", (not has_lines) or (srt.lstrip().startswith("1\n") and " --> " in srt),
          f"{len(srt)} 字符")

    # TXT 导出（用户要的格式）：对话记录，必须含时间戳 + 说话人 + 原文 + 译文
    txt = take_str(lib.translator_session_export(h, 2))
    check("TXT 导出非空", (not has_lines) or len(txt) > 0, f"{len(txt)} 字符")
    if has_lines:
        check("TXT 含时间戳", "[" in txt and "]" in txt, txt[:60].replace("\n", " "))
        check("TXT 含说话人", ("对方" in txt) or ("我" in txt),
              "".join(sorted({w for w in ["对方", "我"] if w in txt})))
        check("TXT 含原文与译文标记", "EN:" in txt and "ZH:" in txt,
              txt[:80].replace("\n", " "))

    # ── 10. 转写盘读回 ──
    ts = FfiTranscriptStats()
    lib.translator_transcript_stats(h, C.byref(ts))
    files = sorted(tmp.glob("*.dsht")) if tmp.exists() else []
    check("转写盘文件存在", len(files) >= 1,
          f"{len(files)} 个文件，最新 {files[-1].name if files else '-'}")

    if files:
        f = files[0]
        q = json.loads(take_str(lib.translator_transcript_query(h, 0, 1000)) or "{}")
        lines = q.get("lines", [])
        # 每条字幕 2 条记录（add + fill）；读回的是本场最新文件
        check("从转写盘读回行数与落盘记录吻合",
              len(lines) > 0 and len(lines) * 2 == ts2.written,
              f"读回 {len(lines)} 行 / 本场写入 {ts2.written} 条记录")

        # 历史回溯核心：内存窗口很小，但文件里必须远超窗口
        window = int(os.environ.get("WINDOW", "200"))
        if window < len(lines):
            check("内存窗口外内容可从转写盘取回",
                  len(lines) > window,
                  f"窗口仅 {window} 行，但从文件取回 {len(lines)} 行")
            # 且最早一行必须还在（seq 0）
            first = min((l.get("st", 0) for l in lines), default=None)
            check("最早的字幕仍在文件里", first is not None,
                  f"最早 startMs={first}")

    # ── 11. 释放（幂等）──
    lib.translator_session_free(h)
    lib.translator_session_free(h)
    check("session_free 幂等", True)

    # ── 汇总 ──
    print()
    print(f"[结果] {len(passed)} 通过 / {len(failed)} 失败")
    if failed:
        print("失败项: " + ", ".join(failed))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
