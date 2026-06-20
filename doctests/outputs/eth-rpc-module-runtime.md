# Running the eth-rpc Module Against logoscore

`logos-evm-eth-rpc-module` is the **proxyable, fail-closed Ethereum JSON-RPC
client** for the Logos multi-chain EVM wallet. It stores configuration per
chain (endpoint + proxy policy) so callers route by `chainId` alone, and every
outbound request is built through a single fail-closed chokepoint: a chain
configured with `proxyRequired` and no usable proxy **refuses to send** rather
than leaking in the clear.

This doc-test drives the module through a `logoscore` daemon against a **local
mock JSON-RPC node** (so it needs no external network and reproduces in CI):

1. Build/install the module and start a daemon.
2. Configure a chain pointing at the mock node and read a balance — a real
   round-trip through the module's RPC transport.
3. Configure a second chain that **requires a proxy** but has none, and watch
   the module refuse the request (the privacy guarantee).

**What you'll build:** This `eth_rpc_module`, packaged as `.lgx`, installed with `lgpm`, and driven through a `logoscore` daemon against a local mock node.

**What you'll learn:**

- How per-chain config is stored in the module and addressed by chainId
- How a JSON-RPC round-trip flows through the module's transport
- How the fail-closed proxy chokepoint refuses to send when a proxy is required but unavailable

## Prerequisites

- **Nix** with flakes enabled. Install from [nixos.org](https://nixos.org/download.html), then enable flakes:

```bash
mkdir -p ~/.config/nix
echo 'experimental-features = nix-command flakes' >> ~/.config/nix/nix.conf
```

- **A Linux or macOS machine** with `python3` available (used to run the local mock JSON-RPC node).

---

## Step 1: Build logoscore and lgpm

### 1.1 Build logoscore

```bash
nix build 'github:logos-co/logos-logoscore-cli#cli' --out-link ./logos
```

### 1.2 Build lgpm

```bash
nix build 'github:logos-co/logos-package-manager#cli' -o lgpm
```

---

## Step 2: Build and install the eth-rpc module

### 2.1 Build the module's .lgx

```bash
nix build 'github:logos-co/logos-evm-eth-rpc-module#lgx' -o eth-rpc-lgx
```

```bash
ls eth-rpc-lgx/*.lgx
```

### 2.2 Seed the capability module

```bash
mkdir -p modules
cp -RL ./logos/modules/. ./modules/

```

### 2.3 Install the .lgx with lgpm

```bash
./lgpm/bin/lgpm --modules-dir ./modules --allow-unsigned install --file eth-rpc-lgx/*.lgx
```

### 2.4 Confirm the install

```bash
./lgpm/bin/lgpm --modules-dir ./modules list
```

---

## Step 3: Start a mock JSON-RPC node

A tiny local node that answers a few JSON-RPC methods with canned values, so
the round-trip is deterministic and offline.

### 3.1 Write the mock node

```
import http.server, json
RES = {"eth_chainId": "0x1", "eth_getBalance": "0x1234", "eth_blockNumber": "0x10"}
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get('content-length', 0))
        req = json.loads(self.rfile.read(n) or b'{}')
        body = json.dumps({"jsonrpc": "2.0", "id": req.get("id", 1),
                           "result": RES.get(req.get("method"), "0x0")}).encode()
        self.send_response(200)
        self.send_header('content-length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a): pass
http.server.HTTPServer(('127.0.0.1', 8599), H).serve_forever()
```

### 3.2 Start the mock node

```bash
python3 mock_node.py &
```

```bash
sleep 2
```

---

## Step 4: Run the daemon and drive the client

### 4.1 Write the chain configs

Two chains: chain 1 points at the mock node with no proxy required;
chain 9 **requires** a proxy but is given none — so it must fail closed.

```json
{ "endpoint": "http://127.0.0.1:8599", "proxyRequired": false }
```

### 4.2 Write the fail-closed chain config

```json
{ "endpoint": "http://127.0.0.1:8599", "proxyRequired": true }
```

### 4.3 Start the daemon

```bash
logoscore -D -m ./modules > logs.txt &
```

```bash
sleep 3
```

### 4.4 Load the module

```bash
./logos/bin/logoscore load-module eth_rpc_module
```

### 4.5 Configure chain 1 (mock node, no proxy)

```bash
logoscore call eth_rpc_module set_chain_config 1 @chain_ok.json
```

### 4.6 List configured chains

```bash
./logos/bin/logoscore call eth_rpc_module list_chains
```

### 4.7 Verify the chain ID (real RPC round-trip)

`verify_chain_id` issues a live `eth_chainId` to the mock node.

```bash
logoscore call eth_rpc_module verify_chain_id 1
```

### 4.8 Read a balance (round-trip)

```bash
logoscore call eth_rpc_module get_balance 1 <address>
```

### 4.9 Configure chain 9 (proxy REQUIRED, none available)

```bash
./logos/bin/logoscore call eth_rpc_module set_chain_config 9 @chain_fc.json
```

### 4.10 Fail-closed: the request is refused

Chain 9 requires a proxy but none is configured, so the module refuses
to send the request in the clear — the wallet's privacy guarantee.

```bash
logoscore call eth_rpc_module get_balance 9 <address>
```

### 4.11 Stop the daemon and the mock node

```bash
./logos/bin/logoscore stop
pkill -f mock_node.py 2>/dev/null || true

```

```bash
sleep 2
```

### 4.12 Confirm the daemon has stopped

```bash
./logos/bin/logoscore status || true
```
