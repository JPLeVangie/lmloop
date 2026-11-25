#!/usr/bin/env python3
"""Count-only model-progress beacon for an OpenAI-style SSE relay. Stdlib only.

Appends the literal bytes b"model\\n" to a precreated bridge file whenever the upstream SSE stream
carries a real text / reasoning / tool-call delta, at most once per second (events inside the
window are dropped, not queued). No upstream text ever reaches the file. Ignored: comments
(keepalives), usage-only chunks, [DONE], empty or role-only deltas, malformed JSON, and anything
over the size caps. The decoder only observes bytes; it never alters, delays or withholds them.

Progress file contract (checked at open; ValueError otherwise): created ahead of time by the
trusted side, regular file, not a symlink, mode 0600 (no group/other bits), owned by this user,
link count 1. Keep it outside the model's reach; this module cannot check that.
Opt-in shared mode (--owner-uid/--writer-gid, see ProgressSink) is for a relay under another account.

Hookup, no edit to bridge.py (the wrapper below patches http.client in the bridge process and
then runs the bridge's own main(), so argv, endpoint, credential handling and byte forwarding are
the bridge's own):

    python3 rust/tools/sse_progress.py --progress-file /path/precreated.0600 \\
        --bridge /tmp/opencode/lm1x4.mus4.CKDUcQ/bridge.py -- <the usual bridge.py args>

Hookup as a library, if bridge.py is ever edited: build one `ProgressSink(path)` at startup, then
per upstream response `d = SseDecoder(sink)` and call `d.feed(chunk)` on each chunk read1()
returns, before `self.wfile.write(chunk)`. feed() never raises.
"""
import argparse, fcntl, json, os, re, runpy, stat, sys, threading, time

MAX_LINE = 1 << 18   # one SSE line; longer lines are skipped whole
MAX_EVENT = 1 << 20  # joined data of one event; larger events are skipped whole
INTERVAL = 1.0
_EOL = re.compile(rb"\r\n|\r|\n")


class ProgressSink:
    """`shared=(owner_uid, writer_gid)` opts in to the relay-account mode, where the backend owns the
    file and this writer reaches it through the group: exactly 0620, those ids, one link, an exclusive
    flock held for our lifetime, and the path must still name our inode and pass the same checks
    before every write, else the sink retires for good. Group write is not kernel append-only: the
    relay is trusted, and the worker must not see the path or belong to the group."""

    def __init__(self, path, clock=time.monotonic, shared=None):
        fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
        self.path, self.shared = path, shared
        try:
            st = os.fstat(fd)
            if not self._trusted(st):
                raise ValueError("progress file must be a private 0600 regular file, owned by us, one link"
                                 if shared is None else
                                 "shared progress file must be a regular 0620 file with the expected owner and gid, one link")
            if shared is not None:
                try:
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    raise ValueError("another writer holds the progress file") from None
        except BaseException:
            os.close(fd)
            raise
        self.ident = (st.st_dev, st.st_ino)
        self.fd, self.clock, self.last, self.lock = fd, clock, None, threading.Lock()

    def _trusted(self, st):
        if not stat.S_ISREG(st.st_mode) or st.st_nlink != 1:
            return False
        if self.shared is None:
            return not st.st_mode & 0o177 and st.st_uid == os.geteuid()
        return stat.S_IMODE(st.st_mode) == 0o620 and (st.st_uid, st.st_gid) == tuple(self.shared)

    def _still_ours(self):
        try:
            disk, st = os.stat(self.path, follow_symlinks=False), os.fstat(self.fd)
        except OSError:
            return False
        return (disk.st_dev, disk.st_ino) == self.ident == (st.st_dev, st.st_ino) and self._trusted(st)

    def _retire(self):
        try:
            os.close(self.fd)  # also drops the flock
        except OSError:
            pass
        self.fd = None

    def tick(self):
        with self.lock:
            now = self.clock()
            if self.fd is None or (self.last is not None and now - self.last < INTERVAL):
                return
            self.last = now
            if self.shared is not None and not self._still_ours():
                return self._retire()
            try:
                os.write(self.fd, b"model\n")
            except OSError:  # progress is best-effort; never disturb the relay
                self._retire()


def _semantic(obj):
    choices = obj.get("choices") if isinstance(obj, dict) else None
    for ch in choices[:16] if isinstance(choices, list) else ():
        if not isinstance(ch, dict):
            continue
        d = ch.get("delta")
        if isinstance(ch.get("text"), str) and ch["text"]:
            return True
        if not isinstance(d, dict):
            continue
        if any(isinstance(d.get(k), str) and d[k] for k in ("content", "reasoning_content", "reasoning")):
            return True
        tcs = d.get("tool_calls")
        for tc in tcs[:16] if isinstance(tcs, list) else ():
            f = tc.get("function") if isinstance(tc, dict) else None
            if isinstance(f, dict) and any(isinstance(f.get(k), str) and f[k] for k in ("name", "arguments")):
                return True
    return False


class SseDecoder:
    """One per upstream response. Incremental: chunk boundaries may fall anywhere."""

    def __init__(self, sink):
        self.sink, self.buf, self.skip_lf, self.scan = sink, bytearray(), False, 0
        self.data, self.size, self.bad, self.overlong = [], 0, False, False

    def feed(self, chunk):
        try:
            self._feed(bytes(chunk))
        except Exception:  # RecursionError/MemoryError-class surprises must not break forwarding
            self.buf.clear()
            self.data, self.size, self.bad = [], 0, True

    def _feed(self, chunk):
        if self.skip_lf and chunk:
            self.skip_lf = False
            if chunk[:1] == b"\n":
                chunk = chunk[1:]
        buf = self.buf
        buf += chunk
        pos = 0
        for m in _EOL.finditer(buf, self.scan):  # resume where the last chunk stopped: no quadratic rescans
            if self.overlong:  # this is the tail of an over-long line already dropped
                self.overlong = False
            else:
                self._line(bytes(buf[pos:m.start()]) if m.start() - pos <= MAX_LINE else None)
            pos = m.end()
            self.skip_lf = m.group() == b"\r" and pos == len(buf)
        del buf[:pos]
        self.scan = len(buf)
        if len(buf) > MAX_LINE:  # unterminated and already too long: drop it, skip to its end
            buf.clear()
            self.scan, self.overlong, self.bad = 0, True, True

    def _line(self, line):
        if line is None:
            self.bad = True
        elif not line:  # blank line dispatches the event
            if not self.bad and self.data:
                self._event(b"\n".join(self.data))
            self.data, self.size, self.bad = [], 0, False
        elif line[:1] != b":":  # comments / keepalives
            field, _, value = line.partition(b":")
            if field == b"data" and not self.bad:
                self.size += len(value) + 1
                if self.size > MAX_EVENT:
                    self.data, self.bad = [], True
                else:
                    self.data.append(value[1:] if value[:1] == b" " else value)

    def _event(self, payload):
        if payload.strip() == b"[DONE]":
            return
        try:
            obj = json.loads(payload)
        except (ValueError, RecursionError):
            return
        if _semantic(obj):
            self.sink.tick()


def wrap_bridge(sink):
    """Make every http.client response tee its read1() bytes into a fresh SseDecoder."""
    import http.client
    orig = http.client.HTTPConnection.getresponse

    def getresponse(self, *a, **k):
        resp = orig(self, *a, **k)
        dec, read1 = SseDecoder(sink), resp.read1

        def tapped(*ra):
            chunk = read1(*ra)
            if chunk:
                dec.feed(chunk)
            return chunk
        resp.read1 = tapped
        return resp
    http.client.HTTPConnection.getresponse = getresponse


def main(argv=None):
    a = argparse.ArgumentParser(description="run bridge.py with a count-only SSE progress beacon")
    a.add_argument("--progress-file", required=True)
    a.add_argument("--bridge", required=True)
    a.add_argument("--owner-uid", type=int, help="opt in to the shared 0620 file: its owning (backend) uid")
    a.add_argument("--writer-gid", type=int, help="opt in to the shared 0620 file: its dedicated writer gid")
    a.add_argument("rest", nargs=argparse.REMAINDER, help="-- then the bridge's own arguments")
    o = a.parse_args(argv)
    if (o.owner_uid is None) != (o.writer_gid is None):
        a.error("--owner-uid and --writer-gid go together")
    shared = None if o.owner_uid is None else (o.owner_uid, o.writer_gid)
    wrap_bridge(ProgressSink(o.progress_file, shared=shared))
    sys.argv = [o.bridge] + [x for i, x in enumerate(o.rest) if not (i == 0 and x == "--")]
    runpy.run_path(o.bridge, run_name="__main__")


if __name__ == "__main__":
    main()
