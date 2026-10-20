"""Offline IP intelligence readers, stdlib only (no pip on a395).

  Xdb      ip2region xdb 2.0/3.0 (IPv4 and IPv6)   github.com/lionsoul2014/ip2region  Apache-2.0 / MIT
  Mmdb     MaxMind DB format reader                 used for DB-IP lite city + ASN     CC BY 4.0
  Ip2Asn   iptoasn.com ip2asn-combined.tsv(.gz)     ASN, country, AS description       PDDL

Nothing here touches the network: every lookup is a file read.
"""
import bisect
import gzip
import ipaddress
import struct


class Xdb:
    HEADER = 256
    VEC_COLS = 256
    VEC_SIZE = 8

    def __init__(self, path):
        with open(path, "rb") as f:
            self.buf = f.read()
        b = self.buf
        self.structure = struct.unpack_from("<H", b, 0)[0]
        self.created_at = struct.unpack_from("<I", b, 4)[0]
        ipver = struct.unpack_from("<H", b, 16)[0] if self.structure >= 3 else 4
        self.v = 6 if ipver == 6 else 4
        self.nbytes = 16 if self.v == 6 else 4
        self.isize = 38 if self.v == 6 else 14

    def search(self, ip):
        a = ipaddress.ip_address(ip)
        if a.version != self.v:
            return None
        packed = a.packed
        idx = packed[0] * self.VEC_COLS * self.VEC_SIZE + packed[1] * self.VEC_SIZE
        s_ptr, e_ptr = struct.unpack_from("<II", self.buf, self.HEADER + idx)
        if not s_ptr or not e_ptr:
            return None
        n = self.nbytes
        lo, hi = 0, (e_ptr - s_ptr) // self.isize
        while lo <= hi:
            m = (lo + hi) >> 1
            p = s_ptr + m * self.isize
            if self.v == 4:
                sip, eip = struct.unpack_from("<II", self.buf, p)
                val = int(a)
            else:
                sip = int.from_bytes(self.buf[p:p + n], "big")
                eip = int.from_bytes(self.buf[p + n:p + 2 * n], "big")
                val = int(a)
            if val < sip:
                hi = m - 1
            elif val > eip:
                lo = m + 1
            else:
                dlen, dptr = struct.unpack_from("<HI", self.buf, p + 2 * n)
                return self.buf[dptr:dptr + dlen].decode("utf-8") if dlen else None
        return None


class Mmdb:
    MARKER = b"\xab\xcd\xefMaxMind.com"

    def __init__(self, path):
        with open(path, "rb") as f:
            self.buf = f.read()
        i = self.buf.rfind(self.MARKER)
        if i < 0:
            raise ValueError("not an mmdb file")
        self.meta, _ = self._decode(i + len(self.MARKER), i + len(self.MARKER))
        self.nodes = self.meta["node_count"]
        self.rsize = self.meta["record_size"]
        self.nbytes = self.rsize * 2 // 8
        self.tree_size = self.nodes * self.nbytes
        self.data_start = self.tree_size + 16
        self.ipv4_start = 0
        if self.meta["ip_version"] == 6:
            node = 0
            for _ in range(96):
                if node >= self.nodes:
                    break
                node = self._read_node(node, 0)
            self.ipv4_start = node

    def _read_node(self, node, bit):
        off = node * self.nbytes
        b = self.buf
        if self.rsize == 24:
            p = off + bit * 3
            return (b[p] << 16) | (b[p + 1] << 8) | b[p + 2]
        if self.rsize == 28:
            if bit == 0:
                return ((b[off + 3] & 0xF0) << 20) | (b[off] << 16) | (b[off + 1] << 8) | b[off + 2]
            return ((b[off + 3] & 0x0F) << 24) | (b[off + 4] << 16) | (b[off + 5] << 8) | b[off + 6]
        if self.rsize == 32:
            return struct.unpack_from(">I", b, off + bit * 4)[0]
        raise ValueError("record size %s" % self.rsize)

    def lookup(self, ip):
        a = ipaddress.ip_address(ip)
        packed = a.packed
        bits = len(packed) * 8
        node = self.ipv4_start if (a.version == 4 and self.meta["ip_version"] == 6) else 0
        depth = 0
        for depth in range(bits):
            if node >= self.nodes:
                break
            bit = (packed[depth >> 3] >> (7 - (depth & 7))) & 1
            node = self._read_node(node, bit)
        if node == self.nodes:
            return None
        if node > self.nodes:
            off = node - self.nodes - 16 + self.data_start
            return self._decode(off, self.data_start)[0]
        return None

    def _decode(self, off, base):
        b = self.buf
        ctrl = b[off]
        off += 1
        typ = ctrl >> 5
        if typ == 1:  # pointer
            ss = (ctrl >> 3) & 3
            vvv = ctrl & 7
            if ss == 0:
                p = (vvv << 8) | b[off]; off += 1
            elif ss == 1:
                p = ((vvv << 16) | (b[off] << 8) | b[off + 1]) + 2048; off += 2
            elif ss == 2:
                p = ((vvv << 24) | (b[off] << 16) | (b[off + 1] << 8) | b[off + 2]) + 526336; off += 3
            else:
                p = struct.unpack_from(">I", b, off)[0]; off += 4
            val, _ = self._decode(base + p, base)
            return val, off
        if typ == 0:
            typ = 7 + b[off]
            off += 1
        size = ctrl & 0x1F
        if size == 29:
            size = 29 + b[off]; off += 1
        elif size == 30:
            size = 285 + ((b[off] << 8) | b[off + 1]); off += 2
        elif size == 31:
            size = 65821 + ((b[off] << 16) | (b[off + 1] << 8) | b[off + 2]); off += 3
        if typ == 2:
            return b[off:off + size].decode("utf-8"), off + size
        if typ == 3:
            return struct.unpack_from(">d", b, off)[0], off + 8
        if typ == 4:
            return bytes(b[off:off + size]), off + size
        if typ in (5, 6, 9, 10):
            return int.from_bytes(b[off:off + size], "big"), off + size
        if typ == 8:
            v = int.from_bytes(b[off:off + size], "big")
            if size == 4 and v & 0x80000000:
                v -= 1 << 32
            return v, off + size
        if typ == 7:
            d = {}
            for _ in range(size):
                k, off = self._decode(off, base)
                v, off = self._decode(off, base)
                d[k] = v
            return d, off
        if typ == 11:
            arr = []
            for _ in range(size):
                v, off = self._decode(off, base)
                arr.append(v)
            return arr, off
        if typ == 14:
            return bool(size), off
        if typ == 15:
            return struct.unpack_from(">f", b, off)[0], off + 4
        raise ValueError("mmdb type %s" % typ)


class Ip2Asn:
    def __init__(self, path):
        op = gzip.open if path.endswith(".gz") else open
        rows = {4: [], 6: []}
        with op(path, "rt", encoding="utf-8", errors="replace") as f:
            for line in f:
                p = line.rstrip("\n").split("\t")
                if len(p) < 5:
                    continue
                try:
                    s = ipaddress.ip_address(p[0])
                    e = ipaddress.ip_address(p[1])
                except ValueError:
                    continue
                rows[s.version].append((int(s), int(e), int(p[2]), p[3], p[4]))
        for v in rows:
            rows[v].sort()
        self.rows = rows
        self.starts = {v: [r[0] for r in rows[v]] for v in rows}

    def lookup(self, ip):
        a = ipaddress.ip_address(ip)
        v = int(a)
        i = bisect.bisect_right(self.starts[a.version], v) - 1
        if i < 0:
            return None
        s, e, asn, cc, desc = self.rows[a.version][i]
        if s <= v <= e and asn:
            return {"asn": asn, "country": cc, "as_desc": desc}
        return None
