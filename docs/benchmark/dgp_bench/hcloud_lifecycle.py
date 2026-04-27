from __future__ import annotations

import os

from .config import APP


def client():
    try:
        from hcloud import Client
    except ImportError as e:
        raise SystemExit("Install Hetzner Cloud SDK first: python3 -m pip install -r docs/benchmark/requirements.txt") from e
    token = os.environ.get("HCLOUD_TOKEN")
    if not token:
        raise SystemExit("HCLOUD_TOKEN is required for lifecycle commands")
    return Client(token=token, application_name=APP, application_version="0.1")


def up(args) -> int:
    from hcloud.images import Image
    from hcloud.locations import Location
    from hcloud.server_types import ServerType
    from hcloud.ssh_keys import SSHKey

    hc = client()
    labels = {"app": APP, "run": args.run_id}
    ssh_keys = [SSHKey(name=args.ssh_key_name)] if args.ssh_key_name else None
    roles = [("single", args.client_type)] if args.single_vm else [("client", args.client_type), ("proxy", args.proxy_type)]
    for role, server_type in roles:
        name = f"dgp-bench-{args.run_id}-{role}"
        existing = hc.servers.get_by_name(name)
        if existing:
            print(f"exists {name} {existing.id}")
            continue
        resp = hc.servers.create(
            name=name,
            server_type=ServerType(name=server_type),
            image=Image(name=args.image),
            location=Location(name=args.location),
            ssh_keys=ssh_keys,
            labels={**labels, "role": role},
            user_data=cloud_init(role),
        )
        print(f"created {role}: id={resp.server.id} name={name} root_password={'set' if resp.root_password else 'ssh-key'}")
    return 0


def status(args) -> int:
    hc = client()
    for server in hc.servers.get_all(label_selector=f"app={APP},run={args.run_id}"):
        ips = []
        if server.public_net and server.public_net.ipv4:
            ips.append(server.public_net.ipv4.ip)
        print(server.id, server.name, server.status, ",".join(ips))
    return 0


def down(args) -> int:
    hc = client()
    for server in hc.servers.get_all(label_selector=f"app={APP},run={args.run_id}"):
        if args.dry_run:
            print(f"would delete {server.id} {server.name}")
        else:
            print(f"delete {server.id} {server.name}")
            server.delete()
    return 0


def cloud_init(role: str) -> str:
    return f"""#cloud-config
package_update: true
packages:
  - python3
  - python3-pip
  - python3-venv
  - curl
  - ca-certificates
  - jq
  - sysstat
  - docker.io
runcmd:
  - systemctl enable --now docker || true
  - echo '{role}' >/etc/dgp-bench-role
"""
