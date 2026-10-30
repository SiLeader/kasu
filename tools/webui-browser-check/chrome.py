import asyncio, json, os, subprocess, sys, time, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__))); import scenario
import websockets
class SyncSocket:
    def __init__(self, url):
        self.loop = asyncio.new_event_loop(); asyncio.set_event_loop(self.loop)
        self.conn = self.loop.run_until_complete(websockets.connect(url, max_size=None))
    def send(self, text): self.loop.run_until_complete(self.conn.send(text))
    def recv(self, timeout=None):
        try:
            return self.loop.run_until_complete(asyncio.wait_for(self.conn.recv(), timeout))
        except asyncio.TimeoutError:
            raise TimeoutError

class Chrome:
    def __init__(self, page_js):
        self.proc = subprocess.Popen(["google-chrome", "--headless=new", "--no-sandbox", "--disable-gpu", "--remote-debugging-port=9333",
            f"--user-data-dir={os.environ.get('TMPDIR', '/tmp')}/kasu-chrome-profile", "about:blank"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(100):
            try:
                targets = json.load(urllib.request.urlopen("http://127.0.0.1:9333/json")); break
            except Exception: time.sleep(0.1)
        page = next(t for t in targets if t["type"] == "page")
        self.sock = SyncSocket(page["webSocketDebuggerUrl"])
        self.id = 0; self.events = []
        self.call("Page.enable"); self.call("Runtime.enable")
        self.call("Network.enable"); self.call("Network.setExtraHTTPHeaders", headers={"Accept-Language": "en"})
        self.js = page_js
        self.call("Emulation.setScriptExecutionDisabled", value=not page_js)
    def call(self, method, **params):
        self.id += 1; me = self.id
        self.sock.send(json.dumps({"id": me, "method": method, "params": params}))
        while True:
            msg = json.loads(self.sock.recv())
            if msg.get("id") == me:
                if "error" in msg: raise RuntimeError(msg["error"])
                return msg.get("result", {})
            self.events.append(msg)
    def wait_load(self, timeout=20):
        end = time.time() + timeout
        while time.time() < end:
            if any(e.get("method") == "Page.loadEventFired" for e in self.events):
                self.events.clear(); return
            try:
                self.events.append(json.loads(self.sock.recv(timeout=0.5)))
            except TimeoutError:
                pass
        raise TimeoutError
    def load(self, url):
        self.events.clear(); self.call("Page.navigate", url=url); self.wait_load()
    def eval(self, script):
        # Page scripts are off; the driver evaluates in an isolated world.
        tree = self.call("Page.getFrameTree")
        world = self.call("Page.createIsolatedWorld", frameId=tree["frameTree"]["frame"]["id"], worldName="driver")
        r = self.call("Runtime.evaluate", expression=script, contextId=world["executionContextId"], returnByValue=True)
        return r.get("result", {}).get("value")
    def submit(self, script):
        self.events.clear(); r = self.eval(script)
        if r != "ok": raise SystemExit("form: " + str(r))
        self.wait_load()
    def key(self, name, navigate=False):
        codes = {"Tab": ("Tab", 9), "Enter": ("Enter", 13)}
        code, vk = codes[name]
        self.events.clear()
        for kind in ("rawKeyDown", "keyUp"):
            params = dict(type=kind, key=name, code=code, windowsVirtualKeyCode=vk, nativeVirtualKeyCode=vk)
            if name == "Enter" and kind == "rawKeyDown":
                self.call("Input.dispatchKeyEvent", type="keyDown", key=name, code=code, windowsVirtualKeyCode=vk, text="\r")
                continue
            self.call("Input.dispatchKeyEvent", **params)
        if navigate:
            self.wait_load()
        else:
            time.sleep(0.05)
    def idle(self): time.sleep(0.01)
    def html(self): return self.eval("document.documentElement.outerHTML") or ""
    def url(self): return self.eval("location.href")
    def close(self): self.proc.terminate()

version = subprocess.run(["google-chrome", "--version"], capture_output=True, text=True).stdout.split()[-1]
c = Chrome(os.environ.get("PAGE_JS") == "1")
try:
    scenario.run(c, sys.argv[1], "Chrome " + version)
    scenario.keyboard(c, sys.argv[1], "Chrome " + version)
finally:
    c.close()
