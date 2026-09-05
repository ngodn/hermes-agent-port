#!/usr/bin/env python3
"""Run the Python endpoint locality predicate without provider/network imports."""
import ast
import ipaddress
import json
from pathlib import Path
import sys
import unicodedata
from urllib.parse import urlparse

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "rust/tools/endpoint-locality-goldens.json"


def generate():
    source = ast.parse((REPO / "agent/model_metadata.py").read_text())
    names = {"_normalize_base_url", "is_local_endpoint"}
    constants = {"_LOCAL_HOSTS", "_CONTAINER_LOCAL_SUFFIXES", "_TAILSCALE_CGNAT"}
    nodes = [n for n in source.body if
             (isinstance(n, ast.FunctionDef) and n.name in names) or
             (isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id in constants for t in n.targets))]
    scope = dict(ipaddress=ipaddress, urlparse=urlparse)
    exec(compile(ast.Module(body=nodes, type_ignores=[]), "agent/model_metadata.py (locality oracle)", "exec"), scope)
    urls = ["", " ", "\u001c\u001f", "http://", "http:///missing", "//localhost", "localhost", "localhost:bad",
            "http://host:bad", "http://host:99999", "http://[broken", "http://[v1.name]/v1",
            "http://[1.2.3.4]", "http://[::ffff:8.8.8.8]", "http://[::ffff:192.168.1.1]",
            "http://[2001:4860:4860::8888]", "http://[fe80::1%eth0]", "http://[::ffff:192.168.1.1%eth0]",
            "http://[::ffff:8.8.8.8%eth0]", "http://localhost?x=1",
            "http://remote.test/?url=http://localhost", "http://localhost@remote.test", "http://remote.test@localhost",
            "http://local\nhost", "http://local\thost", "\u001chttp://localhost/v1\u001f",
            "https://host／evil", "https://host＠evil", "https://host：80", "https://host＃evil",
            "http://10.bad.x.x", "http://10.1.x.x", "http://192.168.bad.address", "http://172.16.999.999",
            "http://100.064.x.x", "http://+10.0.x.x", "http://１０.０.x.x", "http://1_0.0.x.x",
            "http://127.0.0.1%scope", "http://192.168.0.1%scope"]
    hosts = ["localhost", "LOCALHOST", "localhost.", "service", "service.local", "service.internal",
             "host.docker.internal", "host.containers.internal", "host.lima.internal", "x.localhost",
             "docker.internal", "localhost.evil", "8.8.8.8", "127.1", "2130706433", "0x7f000001",
             "010.001.002.003", "127.0.0.1.", "[::1]", "[::]", "[2001:db8::1]"]
    for host in hosts:
        urls.extend([host, f"http://{host}/v1", f"HTTPS://{host}:1234/v1/"])
    # urllib rejects authority characters that normalize into URL delimiters.
    # Compatibility characters such as the account-of sign matter too, not
    # only the visually obvious full-width punctuation.
    for codepoint in range(128, sys.maxunicode + 1):
        char = chr(codepoint)
        if any(delimiter in unicodedata.normalize("NFKC", char) for delimiter in "/?#@:"):
            urls.append(f"http://local{char}host/v1")
        if unicodedata.decimal(char, -1) == 0:
            urls.append(f"http://{chr(codepoint + 1)}{char}.{char}.x.x")
    urls.extend(["http://1__0.0.x.x", "http://_10.0.x.x", "http://10_.0.x.x",
                 "http://10.+.x.x", "http://10." + "9" * 100 + ".x.x",
                 "http://[fe80::1%]", "http://[fe80::1%a%b]"])
    # Include every boundary of the interpreter's private blocks and CGNAT,
    # as well as their immediate neighbours. This catches version-sensitive
    # ipaddress classifications rather than guessing RFC-1918-only behavior.
    networks = list(ipaddress.IPv4Address._constants._private_networks)
    networks.extend(getattr(ipaddress.IPv4Address._constants, "_private_networks_exceptions", []))
    networks.extend([ipaddress.ip_network("100.64.0.0/10"), ipaddress.ip_network("127.0.0.0/8"),
                     ipaddress.ip_network("169.254.0.0/16")])
    for network in networks:
        for address in [int(network.network_address)-1, int(network.network_address),
                        int(network.broadcast_address), int(network.broadcast_address)+1]:
            if 0 <= address <= 0xffffffff:
                urls.append(f"http://{ipaddress.IPv4Address(address)}:11434/v1")
    return json.dumps([dict(url=url, expected=scope["is_local_endpoint"](url)) for url in dict.fromkeys(urls)], indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if OUT.read_text() != content:
            raise SystemExit("Endpoint locality fixtures differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("Usage: gen_endpoint_locality_goldens.py [--check]")
    else:
        OUT.write_text(content)
    print("Verified", len(json.loads(content)), "endpoint locality cases")
