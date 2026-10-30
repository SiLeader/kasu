"""A JavaScript-free browser scenario. A driver loads pages with page scripts
disabled, fills fields and submits forms natively (requestSubmit)."""
import json, re, sys

def fill(fields, form_selector):
    return ("(() => { const f = document.querySelector(%s); if (!f) return 'no form';"
            " for (const [k, v] of Object.entries(%s)) { const e = f.querySelector('[name=\"' + k + '\"]');"
            " if (!e) return 'no field ' + k; if (e.type === 'radio') { f.querySelector('[name=\"' + k + '\"][value=\"' + v + '\"]').checked = true; }"
            " else if (e.type === 'checkbox') { e.checked = !!v; } else { e.value = v; } }"
            " f.requestSubmit(); return 'ok'; })()") % (json.dumps(form_selector), json.dumps(fields))

def run(d, U, name):
    log = []
    def step(label, ok):
        log.append(f"{'PASS' if ok else 'FAIL'} {name}: {label}")
        if not ok:
            print("\n".join(log)); sys.exit(1)
    d.load(U + "/-/preferences")
    d.submit(fill({"lang": "en"}, "form[action='/-/preferences']"))
    step("preferences form sets English", 'lang="en"' in d.html())
    d.load(U + "/demo/app")
    step("anonymous tree renders with README", "Demo" in d.html() and "README.md" in d.html())
    # With scripts, app.js replaces this table (dropping data-latest) after a fetch.
    import time; t = time.time()
    while time.time() - t < 2: d.idle()
    step("page scripts did not run", d.eval("String(document.querySelector('table[data-latest]') !== null)") == "true")
    d.load(U + "/-/login")
    d.submit(fill({"token": "alice-pat"}, "form[action='/-/login']"))
    step("PAT login redirects home signed in", d.url().rstrip('/') == U and "alice" in d.html())
    d.load(U + "/demo/app/-/settings")
    d.submit(fill({"patterns": "refs/heads/main", "approvals": "0", "rebase": "1", "squash": "1"}, "form:has(input[name=section][value=review])"))
    step("review policy saved through the settings form", "refs/heads/main" in d.html())
    d.load(U + "/demo/app/-/edit/main/README.md")
    step("protected branch disables direct landing", 'value="direct" disabled' in d.html().replace("disabled=\"\"", "disabled"))
    d.submit(fill({"content": "# Demo\n\nEdited without JavaScript.", "message": "Edit via " + name, "destination": "review"}, "form.stack"))
    m = re.search(r"/demo/app/-/reviews/(\d+)$", d.url())
    step("edit created a review: " + d.url(), m is not None)
    review = m.group(0)
    d.submit(fill({"body": "Comment from " + name}, "form[action$='/publish']"))
    step("comment published", "Comment from " + name in d.html())
    d.submit(fill({"method": "REBASE"}, "form[action$='/submit']"))
    step("review submitted", "Submitted" in d.html())
    d.load(U + "/demo/app/-/blob/main/README.md")
    step("landed content is visible", "Edited without JavaScript." in d.html())
    d.load(U + "/demo/app/-/log/main")
    step("history shows the landed change", ("Edit via " + name) in d.html())
    print("\n".join(log))

def keyboard(d, U, name):
    """Keyboard-only operation with page scripts disabled: real Tab and
    Enter key events reach the file list, line numbers and diff file list."""
    log = []
    def step(label, ok):
        log.append(f"{'PASS' if ok else 'FAIL'} {name}: keyboard: {label}")
        if not ok:
            print("\n".join(log)); sys.exit(1)
    def tab_to(test, limit=60):
        for _ in range(limit):
            d.key("Tab")
            href = d.eval("document.activeElement && document.activeElement.getAttribute('href') || ''")
            if test(href or ""):
                return True
        return False
    focus_visible = ("(() => { const s = getComputedStyle(document.activeElement);"
                     " return String(s.outlineStyle !== 'none' && parseFloat(s.outlineWidth) > 0); })()")
    d.load(U + "/demo/app")
    step("Tab reaches a file in the listing", tab_to(lambda h: h.endswith("/-/blob/main/README.md")))
    if d.eval("String(document.hasFocus())") == "true":
        step("the focused link has a visible focus indicator", d.eval(focus_visible) == "true")
    else:
        # Without window focus no engine applies :focus; check the rule that will.
        rule = ("String([...document.styleSheets].some(sheet => [...sheet.cssRules].some(r =>"
                " r.selectorText && r.selectorText.split(',').map(x => x.trim()).includes('a:focus')"
                " && /\\bsolid\\b/.test(r.style.getPropertyValue('outline')) && /[1-9]px/.test(r.style.getPropertyValue('outline')))))")
        step("links have a focus outline rule (window not focused here)", d.eval(rule) == "true")
    d.key("Enter", navigate=True)
    step("Enter opens the file", d.url().endswith("/-/blob/main/README.md"))
    d.load(U + "/demo/app/-/blob/main/src/main.rs")
    step("Tab reaches a line number", tab_to(lambda h: h == "#L1"))
    d.key("Enter")
    step("Enter selects the line", d.eval("location.hash") == "#L1"
         and d.eval("String(document.querySelector('tr:target') !== null)") == "true")
    d.load(U + "/demo/app/-/log/main")
    changeset = d.eval("document.querySelector('a[href*=\"/-/changeset/\"]').getAttribute('href')")
    d.load(U + changeset)
    step("Tab reaches a file in the diff list", tab_to(lambda h: h == "#f0"))
    d.key("Enter")
    step("Enter moves to that file's diff", d.eval("location.hash") == "#f0")
    print("\n".join(log))
