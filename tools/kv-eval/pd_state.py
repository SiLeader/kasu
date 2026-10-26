"""Read-only control-plane observations inside the isolated Docker network."""
import json
import sys
import socket
import urllib.request

for host in ['pd1', 'pd2', 'pd3']:
    def get(path):
        with urllib.request.urlopen(f'http://{host}:2379{path}', timeout=2) as response:
            return json.load(response)
    try:
        leader = get('/pd/api/v1/leader')
        regions = get('/pd/api/v1/regions')
        stores = get('/pd/api/v1/stores')
        if not leader.get('name'):
            continue
        alive = {}
        for node in ['tikv1','tikv2','tikv3']:
            try:
                with socket.create_connection((node,20160),timeout=.3):
                    alive[node] = True
            except OSError:
                alive[node] = False
        print(json.dumps({'leader': leader, 'regions': regions['regions'], 'stores': stores['stores'], 'alive':alive}))
        sys.exit(0)
    except Exception:
        continue
sys.exit(1)
