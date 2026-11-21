"""Offline tests for sse_progress.py: python3 -m unittest rust/tools/test_sse_progress.py"""
import http.client, json, os, sys, tempfile, threading, time, unittest
from http.server import BaseHTTPRequestHandler, HTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import sse_progress as sp

SECRET = "SECRET-PROMPT-TEXT-xyz"


def ev(obj):
    return b"data: " + json.dumps(obj).encode() + b"\n\n"


def delta(**d):
    return ev({"choices": [{"index": 0, "delta": d}]})


class Clock:
    t = 100.0

    def __call__(self):
        return self.t


class Base(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="sse_progress.")
        self.dir = self.temp.name
        self.path = os.path.join(self.dir, "progress")
        fd = os.open(self.path, os.O_CREAT | os.O_WRONLY, 0o600)
        os.close(fd)
        self.clock = Clock()
        self.sink = sp.ProgressSink(self.path, self.clock)

    def tearDown(self):
        if self.sink.fd is not None:
            try:
                os.close(self.sink.fd)
            except OSError:
                pass
        self.temp.cleanup()

    def reset(self):
        os.close(self.sink.fd)
        open(self.path, "wb").close()
        self.sink = sp.ProgressSink(self.path, self.clock)

    def written(self):
        with open(self.path, "rb") as f:
            return f.read()

    def count(self, stream, step=1):
        """Feed in `step`-byte pieces, advancing the clock 2s after every event so cadence never hides a hit."""
        d, out = sp.SseDecoder(self.sink), 0
        for i in range(0, len(stream), step):
            before = len(self.written())
            d.feed(stream[i:i + step])
            out += (len(self.written()) - before) // 6
            self.clock.t += 2
        return out


class Decoder(Base):
    def test_semantic_events_count(self):
        for d in ({"content": "hi"}, {"reasoning_content": "t"}, {"reasoning": "t"},
                  {"tool_calls": [{"index": 0, "function": {"name": "bash"}}]},
                  {"tool_calls": [{"index": 0, "function": {"arguments": "{"}}]}):
            self.assertEqual(self.count(delta(**d)), 1, d)
        self.assertEqual(self.count(ev({"choices": [{"text": "legacy"}]})), 1)

    def test_ignored(self):
        s = (b": keepalive\n\n" + delta(role="assistant") + delta(content="") + delta()
             + delta(tool_calls=[{"index": 0, "function": {"arguments": ""}}])
             + ev({"choices": [], "usage": {"total_tokens": 5}}) + b"data: [DONE]\n\n"
             + b"event: ping\ndata:\n\n" + b"data: not json\n\ndata: {\"choices\":\n\n")
        self.assertEqual(self.count(s), 0)
        self.assertEqual(self.written(), b"")

    def test_fragmented_every_split(self):
        s = b": ka\r\n\r\n" + delta(content="a") + delta(role="assistant") + delta(content="b").replace(b"\n", b"\r\n")
        for step in (1, 2, 3, 7):
            self.reset()
            self.assertEqual(self.count(s, step), 2, step)

    def test_crlf_split_and_cr_only(self):
        d = sp.SseDecoder(self.sink)
        d.feed(b'data: {"choices":[{"delta":{"content":"x"}}]}\r')
        d.feed(b"\n\r")
        d.feed(b"\n")
        self.assertEqual(self.written(), b"model\n")
        self.reset()
        sp.SseDecoder(self.sink).feed(b'data: {"choices":[{"delta":{"content":"x"}}]}\r\r')
        self.assertEqual(self.written(), b"model\n")

    def test_multiline_data_joined(self):
        d = sp.SseDecoder(self.sink)
        d.feed(b'data: {"choices":\ndata: [{"delta":{"content":"x"}}]}\n\n')
        self.assertEqual(self.written(), b"model\n")

    def test_cadence_once_per_second(self):
        d = sp.SseDecoder(self.sink)
        for _ in range(50):
            d.feed(delta(content="x"))
        self.assertEqual(self.written(), b"model\n")  # same instant: coalesced
        self.clock.t += 0.99
        d.feed(delta(content="x"))
        self.assertEqual(self.written(), b"model\n")
        self.clock.t += 0.02
        d.feed(delta(content="x"))
        self.assertEqual(self.written(), b"model\nmodel\n")

    def test_no_text_in_file(self):
        self.count(delta(content=SECRET) + delta(reasoning_content=SECRET)
                   + delta(tool_calls=[{"function": {"name": SECRET, "arguments": SECRET}}]))
        self.assertEqual(set(self.written().split(b"\n")), {b"model", b""})
        self.assertNotIn(SECRET.encode(), self.written())

    def test_bounded_garbage(self):
        d = sp.SseDecoder(self.sink)
        for _ in range(40):  # 10 MiB with no newline at all
            d.feed(b"x" * (256 * 1024))
            self.assertLessEqual(len(d.buf), sp.MAX_LINE)
        d.feed(b"\n\n")
        d.feed(delta(content="ok"))  # recovers at the next event
        self.assertEqual(self.written(), b"model\n")

    def test_overlong_line_and_event_drop_that_event_only(self):
        big = b'data: {"choices":[{"delta":{"content":"' + b"a" * (sp.MAX_LINE + 10) + b'"}}]}\n\n'
        self.assertEqual(self.count(big + delta(content="b"), 4096), 1)
        self.reset()
        lines = b"".join(b"data: " + b"z" * 1000 + b"\n" for _ in range(sp.MAX_EVENT // 1000 + 5))
        d = sp.SseDecoder(self.sink)
        d.feed(lines + b"\n")
        self.assertLessEqual(d.size, sp.MAX_EVENT + 1100)
        self.assertEqual(self.written(), b"")

    def test_deep_nesting_does_not_raise(self):
        sp.SseDecoder(self.sink).feed(b"data: " + b"[" * 100000 + b"\n\n")
        self.assertEqual(self.written(), b"")

    def test_odd_shapes_do_not_raise(self):
        for o in (None, 1, "s", [], {"choices": "x"}, {"choices": [1, None]}, {"choices": [{"delta": 3}]},
                  {"choices": [{"delta": {"tool_calls": [None, 1, {"function": 2}]}}]}):
            sp.SseDecoder(self.sink).feed(ev(o))
        self.assertEqual(self.written(), b"")


class Sink(Base):
    def reject(self, make):
        p = os.path.join(self.dir, "bad")
        make(p)
        with self.assertRaises((ValueError, OSError)):
            sp.ProgressSink(p)

    def test_rejects_unsafe_files(self):
        def mode(p):
            open(p, "w").close(); os.chmod(p, 0o640)
        self.reject(mode)
        os.unlink(os.path.join(self.dir, "bad"))
        self.reject(lambda p: os.symlink(self.path, p))
        os.unlink(os.path.join(self.dir, "bad"))
        self.reject(lambda p: os.link(self.path, p))
        os.unlink(os.path.join(self.dir, "bad"))
        self.reject(lambda p: os.mkdir(p, 0o700))
        os.rmdir(os.path.join(self.dir, "bad"))
        self.reject(lambda p: None)  # missing: sink never creates the file

    def test_write_failure_is_silent(self):
        os.close(self.sink.fd)  # simulate a broken descriptor
        sp.SseDecoder(self.sink).feed(delta(content="x"))
        self.assertIsNone(self.sink.fd)


class Stub(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", str(len(self.server.body)))
        self.end_headers()
        self.wfile.write(self.server.body)


class Wrapper(unittest.TestCase):
    def test_read1_bytes_unchanged_and_progress_written(self):
        stream = (b": keepalive\n\n" + delta(role="assistant") + delta(content=SECRET) + b"data: [DONE]\n\n")
        with tempfile.TemporaryDirectory(prefix="sse_progress.") as directory:
            path = os.path.join(directory, "progress")
            fd = os.open(path, os.O_CREAT | os.O_WRONLY, 0o600)
            os.close(fd)
            sink = sp.ProgressSink(path)
            server = HTTPServer(("127.0.0.1", 0), Stub)
            server.body = stream
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                sp.wrap_bridge(sink)
                conn = http.client.HTTPConnection("127.0.0.1", server.server_port)
                conn.request("GET", "/")
                response = conn.getresponse()
                chunks = []
                while chunk := response.read1(7):
                    chunks.append(chunk)
                conn.close()
                self.assertEqual(b"".join(chunks), stream)
                with open(path, "rb") as f:
                    progress = f.read()
                self.assertEqual(progress, b"model\n")
                self.assertNotIn(SECRET.encode(), progress)
            finally:
                os.close(sink.fd)
                server.shutdown()
                server.server_close()


if __name__ == "__main__":
    unittest.main()
