"""Run the JavaScript-free Web UI scenario in real browsers.

Builds a throwaway deployment (kasu-ingest, kasu-server, kasu-web from
target/release) and drives WebKitGTK, Chrome and Firefox with page scripts
disabled. With --page-js the same drivers enable page scripts; the scenario
must then fail at "page scripts did not run" (a control for the check itself).

    cargo build --release -p kasu-server -p kasu-web -p kasu-ingest
    python3 tools/webui-browser-check/run.py [--page-js] [webkit chrome firefox]
"""
import os, shutil, socket, subprocess, sys, tempfile, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
BIN = os.path.join(ROOT, "target", "release")
ACCOUNTS = """{"accounts":[
 {"name":"alice","token_file":"alice.pat","display_name":"Alice","email":"alice@example.com","grants":[{"repositories":["demo/**"],"write_policy":{},"admin":true}]},
 {"name":"public","token_file":"public.pat","grants":[{"repositories":["demo/**"]}]}],
 "delegates":[{"name":"kasu-web","token_file":"web.pat","act_as":["*"],"allow_admin":true,"anonymous_account":"public","trusted_peers":["127.0.0.1/32"]}]}"""

def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]

def deploy(directory):
    """Start a fresh server and web UI; return (url, processes)."""
    def write(name, data):
        with open(os.path.join(directory, name), "wb") as f:
            f.write(data if isinstance(data, bytes) else data.encode())
    for name in ["operator", "alice", "public"]:
        write(f"{name}.pat", f"{name}-pat")
    write("web.pat", "web-delegate")
    write("binding.key", os.urandom(32))
    write("session.key", os.urandom(32))
    write("accounts.json", ACCOUNTS)
    write("web.json", '{"auth":{"anonymous":{"enabled":true,"account":"public"}}}')
    source = os.path.join(directory, "src")
    os.makedirs(os.path.join(source, "src"))
    write("src/README.md", "# Demo\n")
    write("src/src/main.rs", "fn main() {}\n")
    git = ["git", "-c", "user.name=T", "-c", "user.email=t@example.com", "-C", source]
    subprocess.run(["git", "init", "-q", "-b", "main", source], check=True)
    subprocess.run(git + ["add", "."], check=True)
    subprocess.run(git + ["commit", "-qm", "initial"], check=True)
    env = {k: v for k, v in os.environ.items() if not k.startswith(("KASU_POSTGRES", "KASU_S3"))}
    data = os.path.join(directory, "data")
    subprocess.run([f"{BIN}/kasu-ingest", "--create", f"{source}/.git", data, "demo/app"], env=env, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    grpc, http, web = free_port(), free_port(), free_port()
    server_env = dict(env, KASU_TOKEN_FILE=f"{directory}/operator.pat", KASU_ACCOUNTS_FILE=f"{directory}/accounts.json",
                      KASU_BINDING_KEY_FILE=f"{directory}/binding.key")
    log = open(os.path.join(directory, "server.log"), "w")
    server = subprocess.Popen([f"{BIN}/kasu-server", data, f"127.0.0.1:{grpc}", f"127.0.0.1:{http}"], env=server_env,
                              stdout=log, stderr=log)
    url = f"http://127.0.0.1:{web}"
    web_env = dict(env, KASU_WEB_SERVER=f"http://127.0.0.1:{grpc}", KASU_WEB_DELEGATE_TOKEN_FILE=f"{directory}/web.pat",
                   KASU_WEB_SESSION_KEY_FILE=f"{directory}/session.key", KASU_WEB_PUBLIC_URL=url,
                   KASU_WEB_LISTEN=f"127.0.0.1:{web}", KASU_WEB_CONFIG_FILE=f"{directory}/web.json")
    log = open(os.path.join(directory, "web.log"), "w")
    ui = subprocess.Popen([f"{BIN}/kasu-web"], env=web_env, stdout=log, stderr=log)
    for _ in range(100):
        try:
            if urllib.request.urlopen(url + "/-/readyz").read() == b"ready":
                return url, [ui, server]
        except Exception:
            time.sleep(0.1)
    raise SystemExit("the deployment did not become ready")

def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    browsers = args or ["webkit", "chrome", "firefox"]
    env = dict(os.environ, PAGE_JS="1" if "--page-js" in sys.argv else "0")
    failed = []
    for browser in browsers:
        directory = tempfile.mkdtemp(prefix="kasu-browser-")
        url, processes = deploy(directory)
        try:
            result = subprocess.run([sys.executable, os.path.join(HERE, f"{browser}.py"), url], env=env, timeout=300)
            if result.returncode != 0:
                failed.append(browser)
        finally:
            for p in processes:
                p.terminate()
            shutil.rmtree(directory, ignore_errors=True)
    if failed:
        raise SystemExit("failed: " + ", ".join(failed))

if __name__ == "__main__":
    main()
