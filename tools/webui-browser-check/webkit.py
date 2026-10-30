import gi, os, sys, time
gi.require_version("WebKit2", "4.1"); gi.require_version("Gtk", "3.0"); gi.require_version("Gdk", "3.0")
from gi.repository import WebKit2, Gtk, Gdk, GLib
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__))); import scenario

class WebKit:
    def __init__(self):
        Gtk.init([])
        self.view = WebKit2.WebView()
        settings = self.view.get_settings()
        settings.set_enable_javascript_markup(os.environ.get("PAGE_JS") == "1")  # no page scripts by default
        self.window = Gtk.Window(); self.window.set_default_size(1200, 800); self.window.add(self.view); self.window.show_all(); self.window.present()
        self.loaded = False
        self.view.connect("load-changed", lambda v, e: setattr(self, "loaded", e == WebKit2.LoadEvent.FINISHED) if e == WebKit2.LoadEvent.FINISHED or e == WebKit2.LoadEvent.STARTED else None)
    def spin(self, until, timeout=20):
        end = time.time() + timeout
        while not until():
            if time.time() > end: raise TimeoutError
            Gtk.main_iteration_do(False); time.sleep(0.005)
    def load(self, url):
        self.loaded = False; self.view.load_uri(url); self.spin(lambda: self.loaded)
    def eval(self, script):
        box = {}
        def done(view, result):
            try:
                box["v"] = view.evaluate_javascript_finish(result).to_string()
            except Exception as e:
                box["v"] = None; box["e"] = str(e)
        self.view.evaluate_javascript(script, -1, None, None, None, done); self.spin(lambda: "v" in box)
        return box["v"]
    def submit(self, script):
        self.loaded = False; r = self.eval(script)
        if r != "ok": raise SystemExit("form: " + str(r))
        self.spin(lambda: self.loaded)
    def key(self, name, navigate=False):
        keyval = {"Tab": Gdk.KEY_Tab, "Enter": Gdk.KEY_Return}[name]
        self.view.grab_focus()
        if navigate:
            self.loaded = False
        seat = Gdk.Display.get_default().get_default_seat()
        for kind in (Gdk.EventType.KEY_PRESS, Gdk.EventType.KEY_RELEASE):
            event = Gdk.Event.new(kind)
            event.key.window = self.view.get_window()
            event.key.keyval = keyval
            keys = Gdk.Keymap.get_for_display(Gdk.Display.get_default()).get_entries_for_keyval(keyval)[1]
            event.key.hardware_keycode = keys[0].keycode if keys else 0
            event.key.time = Gdk.CURRENT_TIME
            event.set_device(seat.get_keyboard())
            self.view.event(event)
        if navigate:
            self.spin(lambda: self.loaded)
        else:
            end = time.time() + 0.1
            while time.time() < end: self.idle()
    def idle(self): Gtk.main_iteration_do(False); time.sleep(0.01)
    def html(self): return self.eval("document.documentElement.outerHTML") or ""
    def url(self): return self.view.get_uri()

driver = WebKit()
label = "WebKit " + ".".join(map(str, (WebKit2.get_major_version(), WebKit2.get_minor_version())))
scenario.run(driver, sys.argv[1], label)
scenario.keyboard(driver, sys.argv[1], label)
