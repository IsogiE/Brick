"""Validate the proxy and keep unneeded expression matchers outside the active policy."""
import argparse
import json
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument('--image', required=True)
args = parser.parse_args()
config = Path(__file__).resolve().parent.parent / 'presence/Caddyfile'
base = ['docker', 'run', '--rm', '--read-only', '--cap-drop', 'ALL', '--network', 'none',
        '--security-opt', 'no-new-privileges:true', '--tmpfs', '/data:rw,nosuid,nodev',
        '--tmpfs', '/config:rw,nosuid,nodev', '--mount', f'type=bind,src={config},dst=/etc/caddy/Caddyfile,readonly',
        '-e', 'BRICK_DOMAIN=localhost', '-e', 'BRICK_ACME_EMAIL=security@example.invalid', args.image, 'caddy']
result = subprocess.run([*base, 'adapt', '--config', '/etc/caddy/Caddyfile'], text=True, capture_output=True, check=True)
policy = json.loads(result.stdout)
def check(value):
    if isinstance(value, dict):
        assert 'expression' not in value, 'CEL expressions require a Caddy/CEL upgrade and a new security review'
        for item in value.values(): check(item)
    elif isinstance(value, list):
        for item in value: check(item)
check(policy)
subprocess.run([*base, 'validate', '--config', '/etc/caddy/Caddyfile'], check=True, capture_output=True)
print('Proxy configuration is valid and contains no CEL expression matcher.')
