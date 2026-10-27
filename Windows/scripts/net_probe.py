#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""网络通道探测引擎（**只读**——绝不修改任何系统代理设置）。

背景
----
Windows 上的代理软件常被随手开关，端口也会随软件版本变化
（Clash 默认 7890、v2rayN 10808/10809、shadowsocks 1080、
部分版本 7897/2080……）。把某个端口写死进 ``.git/config`` 的
``http.proxy``，一旦用户关掉代理软件或换了端口，git push 立刻
失败（``Failed to connect to github.com port 443``），而且
**要手工改配置才能恢复**。

本脚本的职责：给出一条**当前真实可用**的通道。

策略
----
不靠"端口是否在监听"来猜（沙箱/其他工具注入的代理端口经常在监听，
但走 GitHub 会失败），而是**逐个实测到 GitHub git 端点**：

    https://github.com/<owner>/<repo>.git/info/refs?service=git-receive-pack

只要能收到任何 HTTP 响应（200/401/403 都算），就说明 TLS 握手 +
传输链路通，该候选即为可用。

候选优先级（从高到低）
--------------------
1. 环境变量代理（``HTTPS_PROXY`` / ``HTTP_PROXY`` 等）——用户显式设定
2. Windows 系统代理（**只读** ``HKCU\\...\\Internet Settings``）——
   ``ProxyEnable=1`` 时优先；即使 ``ProxyEnable=0`` 也把
   ``ProxyServer`` 里的端口作为低优先级候选（用户可能只开了代理
   软件而没开"系统代理"开关）
3. 常见本地代理端口（按各家代理软件默认值排列）
4. 直连（不使用任何代理）

安全性
------
* 全程**只读**注册表（``winreg``），不写 ``ProxyEnable`` /
  ``ProxyServer``，不动系统代理开关。
* 不写 ``.git/config``、不写任何凭据文件。
* 只输出通道地址，不输出任何令牌。

用法
----
    python net_probe.py                # 探测（默认针对 origin 的 git 端点）
    python net_probe.py --url <URL>    # 探测指定 URL
    python net_probe.py --list         # 只列出候选，不实测

机器可读输出（stdout 最后一行）
--------------------------------
    RESULT=proxy http://127.0.0.1:7890
    RESULT=direct
    RESULT=none

退出码：0 = 找到可用通道；1 = 全部候选都不可用。
"""

import argparse
import os
import subprocess
import sys
import urllib.error
import urllib.request

# ── 探测参数 ────────────────────────────────────────────────
TIMEOUT = 8.0          # 单个候选的 HTTP 超时（秒）
USER_AGENT = "net-probe/1.0"

# 常见代理软件默认端口（按出现频率排列）。
# 格式：(端口, 描述)。描述仅用于日志，便于用户看懂选中了什么。
COMMON_PROXY_PORTS = [
    (7890, "Clash / Clash Verge (HTTP)"),
    (7891, "Clash (SOCKS)"),
    (7897, "Clash 混合端口"),
    (10808, "v2rayN (HTTP)"),
    (10809, "v2rayN (SOCKS)"),
    (1080, "Shadowsocks / Trojan"),
    (1087, "Shadowsocks 变体"),
    (8118, "Shadowsocks 变体"),
    (2080, "sing-box"),
    (4780, "部分代理工具"),
    (8888, "通用 HTTP 代理"),
]


def log(msg):
    """诊断信息一律走 stderr，stdout 留给机器可读结果。"""
    print(msg, file=sys.stderr)


def _reg_read(name):
    """只读读取 HKCU Internet Settings 的单个值。

    返回 (ProxyEnable:int, ProxyServer:str)；读取失败返回 (0, "")。
    **只读**——本项目不提供任何写入注册表的路径。
    """
    try:
        import winreg
    except ImportError:
        return 0, ""
    try:
        with winreg.OpenKey(
            winreg.HKEY_CURRENT_USER,
            r"Software\Microsoft\Windows\CurrentVersion\Internet Settings",
        ) as k:
            enable = 0
            server = ""
            try:
                enable = int(winreg.QueryValueEx(k, "ProxyEnable")[0])
            except OSError:
                pass
            try:
                server = str(winreg.QueryValueEx(k, "ProxyServer")[0])
            except OSError:
                pass
            return enable, server
    except OSError:
        return 0, ""


def _parse_proxy_server(server):
    """把 ProxyServer 字符串解析成 [(scheme, host, port)]。

    支持的写法：
      * ``127.0.0.1:7890``            → http
      * ``http=127.0.0.1:7890;https=127.0.0.1:7890``
      * ``127.0.0.1:7890;127.0.0.1:7891``
    """
    out = []
    if not server:
        return out
    for part in server.split(";"):
        part = part.strip()
        if not part:
            continue
        scheme = "http"
        if "=" in part:
            scheme, part = part.split("=", 1)
            scheme = scheme.strip().lower()
            part = part.strip()
            if scheme not in ("http", "https", "socks5", "socks5h"):
                scheme = "http"
        if ":" not in part:
            continue
        host, _, port = part.rpartition(":")
        host = host.strip().strip("[]")
        port = port.strip()
        if not host or not port.isdigit():
            continue
        out.append((scheme, host, int(port)))
    return out


def _env_candidates():
    """从环境变量收集代理候选（用户显式设定，优先级最高）。"""
    out = []
    seen = set()
    for key in ("HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"):
        val = os.environ.get(key)
        if not val:
            continue
        val = val.strip()
        if not val:
            continue
        # 去掉可能带的凭据部分（user:pass@host:port）——我们不需要认证
        if "://" in val:
            scheme, rest = val.split("://", 1)
            scheme = scheme.lower()
        else:
            scheme, rest = "http", val
        if "@" in rest:
            rest = rest.rsplit("@", 1)[1]
        key2 = (scheme, rest)
        if key2 in seen:
            continue
        seen.add(key2)
        out.append((scheme, rest, "环境变量"))
    return out


def _candidate_list(target_url):
    """构造按优先级排序的候选通道列表。

    返回 [(scheme, hostport, 描述)]，hostport 形如 ``127.0.0.1:7890``。
    """
    cands = []
    seen = set()

    def add(scheme, hostport, desc):
        key = (scheme, hostport)
        if key in seen:
            return
        seen.add(key)
        cands.append((scheme, hostport, desc))

    # 1) 环境变量
    for scheme, hostport, desc in _env_candidates():
        add(scheme, hostport, desc)

    # 2) Windows 系统代理（只读）
    enable, server = _reg_read("ProxyEnable ProxyServer")
    reg_entries = _parse_proxy_server(server)
    for scheme, host, port in reg_entries:
        # ProxyEnable=1 说明用户明确开了"系统代理"，排在环境变量之后
        desc = "Windows 系统代理" if enable else "Windows 系统代理（已停用，仅端口存活）"
        add(scheme, f"{host}:{port}", desc)

    # 3) 常见端口
    for port, desc in COMMON_PROXY_PORTS:
        add("http", f"127.0.0.1:{port}", desc)

    return cands


def _proxy_url(scheme, hostport):
    if scheme == "http":
        return f"http://{hostport}"
    return f"{scheme}://{hostport}"


def _test(proxy_url, target_url):
    """实测某条通道能否访问 target_url。

    返回 True 表示链路可用（收到任何 HTTP 响应即可）。
    """
    if proxy_url:
        handlers = urllib.request.ProxyHandler(
            {"http": proxy_url, "https": proxy_url}
        )
    else:
        # 空字典 = **显式禁用**所有代理（含环境变量里的），走直连
        handlers = urllib.request.ProxyHandler({})

    opener = urllib.request.build_opener(handlers)
    req = urllib.request.Request(target_url, headers={"User-Agent": USER_AGENT})
    try:
        with opener.open(req, timeout=TIMEOUT) as resp:
            return True, resp.status
    except urllib.error.HTTPError as e:
        # 4xx/5xx 也证明 TCP+TLS 通了（只是权限/路径问题）
        return True, e.code
    except urllib.error.URLError as e:
        return False, str(getattr(e, "reason", e))
    except Exception as e:  # 超时、SSL 错误等
        return False, f"{type(e).__name__}: {e}"


def _test_git(proxy_url, target_url):
    """用**真实的 git** 实测通道，而不是只用 urllib 推断。

    为什么必须这样：git 的代理解析顺序是
    ``http.proxy`` → ``https.proxy`` → ``http_proxy`` 环境变量 →
    ``https_proxy`` 环境变量。只设 ``-c http.proxy=`` **不足以**禁用
    环境变量里的 ``https_proxy``（实测会继续走它并超时）；
    必须同时给 ``http.proxy`` 和 ``https.proxy`` 赋空值。

    另一个实测到的 Git for Windows 特性：走代理时，
    **保留**环境变量里的 ``http_proxy`` 才会用 CONNECT 隧道；
    把环境变量清空后，即使显式给了 ``-c http.proxy=``，
    隧道也会建立失败（报 "option to transmit"）。
    因此测代理时**不删**环境变量，只在测直连时才删。

    urllib 说"能通"不等于 git 能通——两者走不同的 TLS 栈与
    代理解析路径。本函数让探测结果与真实 git 行为一致。
    """
    env = dict(os.environ)
    if not proxy_url:
        # 直连：清掉所有大小写变体的代理环境变量，避免干扰判定
        for key in list(env):
            if key.lower() in ("http_proxy", "https_proxy", "all_proxy"):
                env.pop(key, None)

    cmd = ["git"]
    if proxy_url:
        # 注意顺序：git 对同名 -c 选项**后者覆盖前者**，
        # 所以"先空值禁用、再给具体值"才能让代理真正生效。
        # 反过来（先具体值后空值）会让代理被空值覆盖 → 走直连。
        cmd += ["-c", "http.proxy=", "-c", "https.proxy=",
                "-c", f"http.proxy={proxy_url}", "-c", f"https.proxy={proxy_url}"]
    else:
        cmd += ["-c", "http.proxy=", "-c", "https.proxy="]
    cmd += ["ls-remote", target_url]

    try:
        p = subprocess.run(
            cmd, env=env, capture_output=True, text=True, timeout=TIMEOUT + 10,
        )
    except subprocess.TimeoutExpired:
        return False, "git 超时"
    except FileNotFoundError:
        return False, "找不到 git"

    blob = (p.stdout or "") + (p.stderr or "")
    # ── 关键：区分"连不上"与"连上了但没权限" ──
    # git ls-remote 在仓库需要认证时也会以非 0 退出，但这恰恰证明
    # 通道是通的（TLS 握手完成、请求已到达服务端）。
    # 只有出现下列连接层错误才算通道不可用：
    connect_errors = (
        "Failed to connect to",          # 连不上目标/代理
        "Could not connect to server",
        "Connection refused",
        "Connection timed out",
        "Proxy CONNECT aborted",         # CONNECT 隧道建立失败
        "error in the pull function",
        "Recv failure",
        "Empty reply from server",
        "Failed to connect to github.com",
        "unable to access",              # 仅当同时含连接错误
    )
    auth_ok_markers = (
        "Authentication failed",
        "could not read Username",
        "could not read Password",
        "Permission denied",
        "terminal prompts disabled",
        "Repository not found",
        "Please make sure you have the correct access rights",
        "403 Forbidden",
        "404 Not Found",
    )

    if p.returncode == 0:
        return True, "git ls-remote OK"

    # 已连上但认证失败 → 通道可用
    if any(m in blob for m in auth_ok_markers):
        return True, "已连通（仅需认证）"

    # 明确的连接层失败 → 通道不可用
    for m in connect_errors:
        if m in blob:
            if m == "unable to access" and not any(
                c in blob for c in ("Failed to connect", "Could not connect",
                                   "Connection refused", "Proxy CONNECT aborted")
            ):
                continue
            return False, _last_meaningful_line(blob)

    # 无法归类 → 保守判为不可用（宁可多试下一个候选）
    return False, _last_meaningful_line(blob)


def _last_meaningful_line(blob):
    """从 git 输出里挑一行有信息的错误（跳过空行与提示行）。"""
    lines = [l.strip() for l in blob.strip().splitlines() if l.strip()]
    for line in reversed(lines):
        if line.startswith(("fatal:", "error:")) or "Failed to" in line or "unable to" in line:
            return line[:160]
    return lines[-1][:160] if lines else "未知错误"




def _git_endpoint_from_remote():
    """从 origin 的 remote URL 推出可直接交给 git ls-remote 的地址。"""
    try:
        url = subprocess.run(
            ["git", "remote", "get-url", "origin"],
            capture_output=True, text=True, timeout=15,
        ).stdout.strip()
    except Exception:
        url = ""
    if not url:
        return "https://github.com/MEFEason/translator.git"
    if url.startswith("git@") and ":" in url:
        # git@github.com:owner/repo.git → https://github.com/owner/repo.git
        hostpath = url.split("@", 1)[1].replace(":", "/", 1)
        url = f"https://{hostpath}"
    if not url.endswith(".git"):
        url = url + ".git"
    return url


def _probe_target():
    """探测目标：优先用真实 remote URL（git 能直接用），退回 info/refs 端点。

    返回 (git_url, http_probe_url)
    """
    git_url = _git_endpoint_from_remote()
    http_url = git_url + "/info/refs?service=git-receive-pack"
    return git_url, http_url



def main():
    global TIMEOUT

    ap = argparse.ArgumentParser(description="探测可用的 GitHub 网络通道（只读，不改系统代理）")
    ap.add_argument("--url", help="探测目标 URL（默认取 origin 的 git 地址）")
    ap.add_argument("--list", action="store_true", help="只列出候选，不实测")
    ap.add_argument("--timeout", type=float, default=TIMEOUT, help="单候选超时秒数")
    ap.add_argument("--no-git", action="store_true",
                    help="只用 HTTP 探测（不跑 git ls-remote），速度更快但与 git 行为可能不一致")
    args = ap.parse_args()

    TIMEOUT = args.timeout

    git_url, http_url = _probe_target()
    if args.url:
        # --url 同时作用于 HTTP 探测与 git 判定，
        # 否则两者目标不一致会出现"HTTP 说通了、git 却失败"的假象。
        git_url = args.url
        http_url = args.url
    target = git_url
    log(f"探测目标: {target}")

    enable, server = _reg_read("ProxyEnable ProxyServer")
    log(f"系统代理（只读）: ProxyEnable={enable}  ProxyServer={server or '(空)'}")

    cands = _candidate_list(target)

    if args.list:
        for scheme, hostport, desc in cands:
            log(f"  候选 {scheme}://{hostport}  [{desc}]")
        print("RESULT=none")
        return 0

    log(f"共 {len(cands)} 条候选，逐个实测：")

    def probe(purl, desc):
        """实测一条通道。返回 (ok, detail)。"""
        if args.no_git:
            return _test(purl, http_url)
        # 用真实 git 判定——保证结果与实际 push/pull 行为一致
        return _test_git(purl, git_url)

    for scheme, hostport, desc in cands:
        purl = _proxy_url(scheme, hostport)
        ok, detail = probe(purl, desc)
        if ok:
            log(f"  ✓ 可用  {purl}  [{desc}]  {detail}")
            print(f"RESULT=proxy {purl}")
            return 0
        log(f"  ✗ 不可用 {purl}  [{desc}]  {detail}")

    # 最后试直连
    log("  · 尝试直连（不走任何代理）")
    ok, detail = probe(None, "直连")
    if ok:
        log(f"  ✓ 可用  直连  {detail}")
        print("RESULT=direct")
        return 0
    log(f"  ✗ 不可用  直连  {detail}")

    print("RESULT=none")
    return 1


if __name__ == "__main__":
    sys.exit(main())
