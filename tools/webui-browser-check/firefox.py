import asyncio, json, os, subprocess, sys, time, shutil
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__))); import scenario
import websockets

class Firefox:
    def __init__(self, page_js):
        profile = f"{os.environ.get('TMPDIR', '/tmp')}/kasu-ff-profile"; shutil.rmtree(profile, ignore_errors=True); os.makedirs(profile)
        with open(f"{profile}/user.js", "w") as f:
            f.write(f'user_pref("javascript.enabled", {"true" if page_js else "false"});\n')
            f.write('user_pref("intl.accept_languages", "en");\n')
        self.proc = subprocess.Popen(["firefox", "--headless", "--no-remote", "--profile", profile, "--remote-debugging-port=9334"],
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.loop = asyncio.new_event_loop(); asyncio.set_event_loop(self.loop)
        for _ in range(200):
            try:
                self.conn = self.loop.run_until_complete(websockets.connect("ws://127.0.0.1:9334/session", max_size=None)); break
            except Exception: time.sleep(0.1)
        self.id = 0; self.events = []
        self.call("session.new", capabilities={})
        self.call("session.subscribe", events=["browsingContext.load"])
        self.context = self.call("browsingContext.getTree")["contexts"][0]["context"]
    def recv(self, timeout):
        try:
            return json.loads(self.loop.run_until_complete(asyncio.wait_for(self.conn.recv(), timeout)))
        except asyncio.TimeoutError:
            raise TimeoutError
    def call(self, method, **params):
        self.id += 1; me = self.id
        self.loop.run_until_complete(self.conn.send(json.dumps({"id": me, "method": method, "params": params})))
        while True:
            msg = self.recv(30)
            if msg.get("id") == me:
                if msg.get("type") == "error": raise RuntimeError(msg)
                return msg.get("result", {})
            self.events.append(msg)
    def wait_load(self, timeout=20):
        end = time.time() + timeout
        while time.time() < end:
            if any(e.get("method") == "browsingContext.load" for e in self.events):
                self.events.clear(); return
            try: self.events.append(self.recv(0.5))
            except TimeoutError: pass
        raise TimeoutError
    def load(self, url):
        self.events.clear(); self.call("browsingContext.navigate", context=self.context, url=url, wait="complete"); self.events.clear()
    def eval(self, script):
        r = self.call("script.evaluate", expression=script, target={"context": self.context, "sandbox": "driver"}, awaitPromise=False)
        return r.get("result", {}).get("value")
    def submit(self, script):
        self.events.clear(); r = self.eval(script)
        if r != "ok": raise SystemExit("form: " + str(r))
        self.wait_load()
    def key(self, name, navigate=False):
        value = {"Tab": "\uE004", "Enter": "\uE007"}[name]
        self.events.clear()
        self.call("input.performActions", context=self.context, actions=[{"type": "key", "id": "keyboard",
            "actions": [{"type": "keyDown", "value": value}, {"type": "keyUp", "value": value}]}])
        if navigate:
            self.wait_load()
        else:
            time.sleep(0.05)
    def idle(self): time.sleep(0.01)
    def html(self): return self.eval("document.documentElement.outerHTML") or ""
    def url(self): return self.eval("location.href")
    def close(self): self.proc.terminate()

version = subprocess.run(["firefox", "--version"], capture_output=True, text=True).stdout.split()[-1]
f = Firefox(os.environ.get("PAGE_JS") == "1")
try:
    scenario.run(f, sys.argv[1], "Firefox " + version)
    scenario.keyboard(f, sys.argv[1], "Firefox " + version)
finally:
    f.close()
