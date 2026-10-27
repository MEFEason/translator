#!/usr/bin/env python3
"""读取 Windows PE 文件的 FileVersion（纯标准库，无外部依赖）。

用途：打包时校验 onnxruntime.dll 版本。
不能用 PowerShell —— Git Bash 里调 powershell 会被沙箱拦，
而且路径要转 Windows 格式，脆。

用法：
    python pe_version.py <文件路径> [<文件路径> ...]
    成功时每行输出一行 "<路径>\\t<版本>"
    任一文件解析失败 → exit 1
"""

import struct
import sys


def pe_file_version(path: str) -> str:
    """从 PE 版本资源解析 FileVersion，形如 '1.27.1.0'。"""
    with open(path, "rb") as f:
        data = f.read()

    pe_off = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe_off : pe_off + 4] != b"PE\0\0":
        raise ValueError("不是有效的 PE 文件")

    coff = pe_off + 4
    n_sections = struct.unpack_from("<H", data, coff + 2)[0]
    opt_size = struct.unpack_from("<H", data, coff + 16)[0]
    opt = coff + 20
    magic = struct.unpack_from("<H", data, opt)[0]

    # 数据目录偏移：PE32 = 96, PE32+ = 112
    dd = opt + (96 if magic == 0x10B else 112)
    # 索引 2 = Resource Directory
    rsrc_rva, _rsrc_size = struct.unpack_from("<II", data, dd + 2 * 8)
    if not rsrc_rva:
        raise ValueError("PE 无资源段（无版本信息）")

    sections = []
    sec_off = opt + opt_size
    for i in range(n_sections):
        s = sec_off + i * 40
        vsize, vaddr, rsize, raddr = struct.unpack_from("<IIII", data, s + 8)
        sections.append((vaddr, vsize, raddr, rsize))

    def rva2off(rva: int):
        for vaddr, vsize, raddr, rsize in sections:
            if vaddr <= rva < vaddr + max(vsize, rsize):
                return raddr + (rva - vaddr)
        return None

    rsrc_off = rva2off(rsrc_rva)
    if rsrc_off is None:
        raise ValueError("资源段 RVA 无法映射到文件偏移")

    def dir_entries(off: int):
        num_named, num_id = struct.unpack_from("<HH", data, off + 12)
        base = off + 16
        out = []
        for i in range(num_named + num_id):
            eo = base + i * 8
            name_id, rva2 = struct.unpack_from("<II", data, eo)
            out.append((name_id, rva2))
        return out

    def find_version(off: int, depth: int):
        for name_id, rva2 in dir_entries(off):
            if depth == 0 and name_id != 16:  # RT_VERSION = 16
                continue
            if rva2 & 0x80000000:
                res = find_version(rsrc_off + (rva2 & 0x7FFFFFFF), depth + 1)
                if res:
                    return res
            else:
                leaf = rsrc_off + rva2
                if depth == 0:
                    res = find_version(leaf, depth + 1)
                    if res:
                        return res
                else:
                    d_rva, d_size = struct.unpack_from("<II", data, leaf)
                    do = rva2off(d_rva)
                    if do is None:
                        continue
                    return do, d_size
        return None

    found = find_version(rsrc_off, 0)
    if not found:
        raise ValueError("未找到 RT_VERSION 资源")

    voff, vsize = found
    # VS_FIXEDFILEINFO 签名 0xFEEF04BD（小端 = BD 04 EF FE）
    idx = data.find(b"\xbd\x04\xef\xfe", voff, voff + vsize)
    if idx < 0:
        raise ValueError("未找到 VS_FIXEDFILEINFO")

    ms, ls = struct.unpack_from("<II", data, idx + 8)
    return f"{ms >> 16}.{ms & 0xFFFF}.{ls >> 16}.{ls & 0xFFFF}"


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    failed = False
    for path in sys.argv[1:]:
        try:
            print(f"{path}\t{pe_file_version(path)}")
        except Exception as e:  # noqa: BLE001
            print(f"{path}\tERROR: {e}", file=sys.stderr)
            failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
