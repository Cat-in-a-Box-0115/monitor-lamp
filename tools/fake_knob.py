"""A fake knob for testing Detent's link without the hardware.

Needs a virtual COM-port pair (com0com, https://com0com.sourceforge.net) and pyserial (pip install pyserial).
Detent looks for the knob's USB VID/PID, which a com0com port doesn't have, so start Detent with
DETENT_KNOB_PORT=COMx (the other end of the pair) to point it at the port directly.

  python tools/fake_knob.py COM11

Prints every message Detent sends, and sends LAMP_STATE with a new brightness every 5 s, the way the
real knob reports a change it made to the lamp. It also answers a firmware update like the real knob
(the received image is saved as fake_knob_fw.bin), and takes keys in its console window:

  m  mute / unmute the mic           [ ]  monitor brightness -10 / +10
  v  first app's volume -10          t T  turn +1 / -1 (scrolls or zooms in an app mode)
"""
import struct, sys, time, zlib


def cobs_encode(data: bytes) -> bytes:
    out, block = bytearray(), bytearray()
    for b in data:
        if b == 0:
            out += bytes([len(block) + 1]) + block; block = bytearray()
        else:
            block.append(b)
            if len(block) == 254:
                out += bytes([255]) + block; block = bytearray()
    out += bytes([len(block) + 1]) + block
    return bytes(out)

def cobs_decode(data: bytes) -> bytes:
    out, i = bytearray(), 0
    while i < len(data):
        code = data[i]
        if code == 0 or i + code > len(data):
            raise ValueError("bad cobs")
        out += data[i + 1:i + code]; i += code
        if code < 255 and i < len(data):
            out.append(0)
    return bytes(out)

def frame(ty: int, payload: bytes) -> bytes:
    return cobs_encode(bytes([ty]) + struct.pack("<H", len(payload)) + payload) + b"\0"

def s(text: str) -> bytes:
    b = text.encode()[:255]
    return bytes([len(b)]) + b

def strs(p: bytes, n: int):
    out = []
    for _ in range(n):
        l = p[0]; out.append(p[1:1 + l].decode(errors="replace")); p = p[1 + l:]
    return out, p

def describe(ty: int, p: bytes) -> str:
    if ty == 0x01: return f"HELLO proto={p[0]} app={strs(p[5:], 1)[0][0]}"
    if ty == 0x10:
        playing, pos, dur = struct.unpack("<BII", p[:9]); (t, a), _ = strs(p[9:], 2)
        return f"NOW_PLAYING {'>' if playing else '||'} {pos/1000:.1f}/{dur/1000:.0f}s  {t} - {a}"
    if ty == 0x11:
        art, w, h, c, n = struct.unpack("<IHHHH", p[:12]); return f"COVER {art:08x} {w}x{h} chunk {c + 1}/{n}"
    if ty == 0x12:
        lid = struct.unpack("<I", p[:4])[0]; (pr, cu, nx), _ = strs(p[4:], 3); return f"LYRICS #{lid}: {cu}"
    if ty == 0x13: return "LYRICS_NONE"
    if ty == 0x14:
        v = ["-" if x == 255 else x for x in p[:5]]; return f"PC_STATS cpu {v[0]}% {v[1]}C  gpu {v[2]}% {v[3]}C  ram {v[4]}%  {strs(p[5:], 1)[0][0]}"
    if ty == 0x20: (ssid, _pw, ip, _tok), _ = strs(p, 4); return f"PROVISION ssid={ssid} lamp={ip} (password and token hidden)"
    if ty == 0x21: u, tz = struct.unpack("<Ih", p); return f"TIME {time.strftime('%Y-%m-%d %H:%M:%S', time.gmtime(u + tz * 60))}"
    if ty == 0x15: return "AUDIO_APPS " + (", ".join(f"{a['name']} {a['vol']}%{' muted' if a['muted'] else ''}" for a in apps_of(p)) or "(none)")
    if ty == 0x16: return "MIC " + {0: "on", 1: "muted"}.get(p[0], "none")
    if ty == 0x17: return "MONITOR " + ("no ddc" if p[0] == 255 else f"{p[0]}%")
    if ty == 0x18: return f"FOCUS mode {['volume', 'scroll', 'zoom', 'sideways'][p[0]] if p[0] < 4 else p[0]}  {strs(p[1:], 1)[0][0]}"
    if ty == 0x40: size, crc = struct.unpack("<II", p[:8]); return f"FW_BEGIN {size} bytes crc {crc:08x} version {strs(p[8:], 1)[0][0]}"
    if ty == 0x42: return "FW_END"
    return f"type {ty:#04x}, {len(p)} bytes"

def apps_of(p: bytes):
    out, n, p = [], p[0], p[1:]
    for _ in range(n):
        i, vol, muted = struct.unpack("<IBB", p[:6]); (name,), p = strs(p[6:], 1)
        out.append({"id": i, "vol": vol, "muted": bool(muted), "name": name})
    return out

def fw_status(state: int, offset: int, text: str = "") -> bytes:
    return frame(0x43, struct.pack("<BI", state, offset) + s(text))

def key():
    """A key pressed in this console, if any (Windows only)"""
    try:
        import msvcrt
    except ImportError:
        return None
    return msvcrt.getwch() if msvcrt.kbhit() else None

def main():
    import serial  # pyserial
    port = serial.Serial(sys.argv[1], 115200, timeout=0.05)
    port.write(frame(0x01, bytes([2]) + struct.pack("<I", 0) + s("fake-0.2")))
    buf, bright, next_state = bytearray(), 30, time.time() + 5
    apps, mic, monitor, fw, mode = [], False, 50, None, 0
    while True:
        buf += port.read(4096)
        while 0 in buf:
            i = buf.index(0); raw = bytes(buf[:i]); del buf[:i + 1]
            if not raw: continue
            try:
                d = cobs_decode(raw); ty, n = d[0], struct.unpack("<H", d[1:3])[0]
                if n != len(d) - 3: print("bad length"); continue
                p = d[3:]
                if ty == 0x15: apps = apps_of(p)
                if ty == 0x16: mic = p[0] == 1
                if ty == 0x17 and p[0] != 255: monitor = p[0]
                if ty == 0x18: mode = p[0]
                if ty == 0x41:  # FW_DATA: print every 64th chunk only
                    off = struct.unpack("<I", p[:4])[0]
                    if fw is not None and off == len(fw["data"]): fw["data"] += p[4:]
                    if off // 1024 % 64 == 0: print(f"FW_DATA at {off}")
                    port.write(fw_status(0, len(fw["data"]) if fw else 0))
                    continue
                if ty == 0x40:
                    size, crc = struct.unpack("<II", p[:8]); fw = {"size": size, "crc": crc, "data": bytearray()}
                    port.write(fw_status(0, 0))
                if ty == 0x42 and fw is not None:
                    good = len(fw["data"]) == fw["size"] and zlib.crc32(fw["data"]) == fw["crc"]
                    open("fake_knob_fw.bin", "wb").write(fw["data"])
                    port.write(fw_status(2 if good else 1, len(fw["data"]), "" if good else "crc mismatch"))
                    print("firmware received:", "ok, saved as fake_knob_fw.bin" if good else "CRC MISMATCH")
                if ty != 0x11 or p[8:10] == b"\0\0":  # print the first chunk of each cover only
                    print(describe(ty, p))
            except Exception as e:
                print("bad frame:", e)
        if time.time() > next_state:
            bright = 30 if bright >= 90 else bright + 20
            port.write(frame(0x30, struct.pack("<BBBHBI", 0x02, 1, bright, 4000, 0, 0)))  # changed = brightness
            print(f"-> LAMP_STATE bright {bright}%")
            next_state = time.time() + 5
        k = key()
        if k == "m":
            port.write(frame(0x33, bytes([0 if mic else 1]))); print(f"-> SET_MIC {'on' if mic else 'muted'}")
        elif k in ("[", "]"):
            monitor = max(0, min(100, monitor + (10 if k == "]" else -10)))
            port.write(frame(0x34, bytes([monitor]))); print(f"-> SET_MONITOR {monitor}%")
        elif k == "v" and apps:
            a = apps[0]; vol = max(0, a["vol"] - 10)
            port.write(frame(0x32, struct.pack("<IBB", a["id"], vol, 0))); print(f"-> SET_APP_VOLUME {a['name']} {vol}%")
        elif k in ("t", "T"):
            port.write(frame(0x35, struct.pack("<Bb", mode, 1 if k == "t" else -1))); print(f"-> TURN {1 if k == 't' else -1} in mode {mode}")

if __name__ == "__main__":
    main()
