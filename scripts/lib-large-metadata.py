#!/usr/bin/env python3
"""Large-metadata bridge E2E driver; large values stay in files/stdin, not argv."""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time
import urllib.request


ENV = os.environ
OUT = Path(ENV["EVIDENCE_DIR"])
TOKEN_TOPIC = "0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b"
DEPOSIT_WEI = 10**15
EXIT_UNITS = 20_000
EXIT_WEI = EXIT_UNITS * 10**10
GAS_CAP = 16_777_216


def log(message):
    print(f"[large-metadata] {message}", flush=True)


def run(args, *, input=None, timeout=900):
    result = subprocess.run(args, input=input, text=True, capture_output=True,
                            timeout=timeout, cwd=ENV["PROJECT_DIR"])
    if result.returncode:
        (OUT / f"command-error-{time.time_ns()}.log").write_text(result.stdout + result.stderr)
        raise RuntimeError(f"{args[0]} exited {result.returncode}: {result.stderr[-300:]}")
    return result.stdout


def rpc(url, method, params):
    request = urllib.request.Request(url, json.dumps({"jsonrpc": "2.0", "id": 1,
        "method": method, "params": params}).encode(), {"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=90) as response:
        value = json.load(response)
    if "error" in value:
        raise RuntimeError(str(value["error"]))
    return value["result"]


def wait(label, sample, timeout=900):
    deadline = time.monotonic() + timeout
    last_error = None
    while time.monotonic() < deadline:
        try:
            value = sample()
            if value:
                log(f"PASS: {label}")
                return value
        # Restarting the proxy can close an accepted HTTP connection before it
        # serves a response. Retry transport errors within the same deadline.
        except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
            last_error = str(error)[:200]
        time.sleep(3)
    raise AssertionError(f"Timed out: {label}; last error: {last_error}")


def pg(sql):
    return run(["docker", "exec", ENV["PG_CONTAINER"], "psql", "-v", "ON_ERROR_STOP=1",
                "-U", "agglayer", "-d", "agglayer_store", "-tA", "-c", sql]).strip()


def row(token):
    value = pg("SELECT json_build_object('faucet_id',faucet_id,'origin_address',"
        "encode(origin_address,'hex'),'origin_network',origin_network,'symbol',symbol,"
        "'origin_decimals',origin_decimals,'miden_decimals',miden_decimals,'scale',scale,"
        "'metadata',encode(metadata,'hex')) FROM faucet_registry "
        f"WHERE origin_network=0 AND origin_address=decode('{token[2:]}','hex')")
    return json.loads(value) if value else None


def token_balance(token):
    data = run(["cast", "calldata", "balanceOf(address)", ENV["FUNDED_ADDR"]]).strip()
    return int(rpc(ENV["L1_RPC"], "eth_call", [{"to": token, "data": data}, "latest"]), 16)


def wallet_balance(faucet):
    value = run(["bash", "-c", 'iso_wallet_balance "$1" "$2"', "large-metadata",
                 ENV["BRIDGE_ID"], faucet]).strip()
    return int(value) if value.isdecimal() else -1


def metadata_from_event(data):
    data = bytes.fromhex(data.removeprefix("0x"))
    offset = int.from_bytes(data[6*32:7*32], "big")
    length = int.from_bytes(data[offset:offset+32], "big")
    metadata = data[offset+32:offset+32+length]
    assert len(metadata) == length
    return metadata


def event_rows(token):
    # All BridgeEvent fields are non-indexed. Match the exact origin word,
    # then compare the complete stored event across restore (including index).
    value = pg("SELECT COALESCE(json_agg(v ORDER BY block_number,log_index),'[]') FROM "
        "(SELECT block_number,log_index,transaction_hash,data FROM synthetic_logs "
        f"WHERE topics[1]='{TOKEN_TOPIC}' AND substring(data FROM 155 FOR 40)="
        f"'{token[2:].lower()}') v")
    return json.loads(value)


def ready():
    with urllib.request.urlopen(ENV["L2_RPC"] + "/health", timeout=10) as response:
        return response.status == 200


def compose(*args, timeout=900):
    return run(["docker", "compose", "-f", "docker-compose.e2e.yml", "--env-file",
                "fixtures/.env", *args], timeout=timeout)


def main():
    (OUT / "result.json").unlink(missing_ok=True)
    name_bytes = int(ENV["NAME_BYTES"])
    assert name_bytes >= 262_144, "Use a huge payload that exceeds the old transport limit"
    log(f"Deploying generated-name ERC-20 ({name_bytes} name bytes)")
    fixture_root = str(Path(ENV["LARGE_METADATA_FIXTURE"].split(":")[0]).resolve().parent.parent)
    deployment = run(["forge", "create", "--root", fixture_root, ENV["LARGE_METADATA_FIXTURE"], "--rpc-url",
        ENV["L1_RPC"], "--private-key", ENV["FUNDED_KEY"], "--broadcast",
        "--constructor-args", str(name_bytes)])
    (OUT / "deployment.log").write_text(deployment)
    token = re.search(r"Deployed to:\s*(0x[0-9a-fA-F]{40})", deployment).group(1)
    run(["cast", "send", token, "approve(address,uint256)", ENV["BRIDGE_ADDRESS"],
         str(DEPOSIT_WEI), "--rpc-url", ENV["L1_RPC"], "--private-key", ENV["FUNDED_KEY"]])
    receipt = json.loads(run(["cast", "send", ENV["BRIDGE_ADDRESS"],
        "bridgeAsset(uint32,address,uint256,address,bool,bytes)", "1", ENV["DEST_ADDR"],
        str(DEPOSIT_WEI), token, "true", "0x", "--gas-limit", str(GAS_CAP),
        "--rpc-url", ENV["L1_RPC"], "--private-key", ENV["FUNDED_KEY"], "--json"]))
    assert int(receipt["status"], 16) == 1
    (OUT / "l1-deposit-receipt.json").write_text(json.dumps(receipt))
    event = next(log for log in receipt["logs"] if log["address"].lower() ==
        ENV["BRIDGE_ADDRESS"].lower() and log["topics"][0].lower() == TOKEN_TOPIC)
    metadata = metadata_from_event(event["data"])
    assert len(metadata) == 192 + ((name_bytes + 31) // 32) * 32
    assert len(metadata) > 256 * 1024
    name_offset = int.from_bytes(metadata[:32], "big")
    assert int.from_bytes(metadata[name_offset:name_offset+32], "big") == name_bytes
    pattern = b"0123456789ABCDEFGHIJKLMNOPQRSTUV"
    expected_name = (pattern * ((name_bytes + 31) // 32))[:name_bytes]
    assert metadata[name_offset+32:name_offset+32+name_bytes] == expected_name
    padded_name = ((name_bytes + 31) // 32) * 32
    def word(value):
        return value.to_bytes(32, "big")
    expected_metadata = (word(96) + word(128 + padded_name) + word(18)
        + word(name_bytes) + expected_name + bytes(padded_name - name_bytes)
        + word(4) + b"HUGE" + bytes(28))
    assert metadata == expected_metadata, "L1 event differs from canonical ABI preimage"
    digest = run(["cast", "keccak"], input="0x" + metadata.hex()).strip()
    (OUT / "metadata.bin").write_bytes(metadata)
    log(f"Real L1 deposit emitted {len(metadata)} bytes; gas={int(receipt['gasUsed'],16)}/{GAS_CAP}; keccak={digest}")
    registered = wait("huge deposit auto-claimed and faucet registered", lambda: row(token))
    assert bytes.fromhex(registered["metadata"]) == metadata, "claim path discarded/changed preimage"
    faucet = registered["faucet_id"]
    wait("exact Miden minted balance", lambda: wallet_balance(faucet) == 100_000)
    identity = {key: value for key, value in registered.items() if key != "metadata"}
    (OUT / "identity.json").write_text(json.dumps(identity, indent=2))
    initial_l1 = token_balance(token)

    def bridge_out(label, count):
        output = run(["bash", "-c", 'iso_tool "$@"', "large-metadata", "--wallet-id",
            ENV["WALLET_ID"], "--bridge-id", ENV["BRIDGE_ID"], "--faucet-id", faucet,
            "--amount", str(EXIT_UNITS), "--dest-address", ENV["FUNDED_ADDR"], "--dest-network", "0"])
        (OUT / f"{label}-bridge-out.log").write_text(output)
        events = wait(f"{label}: BridgeEvent emitted", lambda: events_at_count(token, count))
        assert all(metadata_from_event(event["data"]) == metadata for event in events)
        assert bytes.fromhex(row(token)["metadata"]) == metadata
        wait(f"{label}: L1 autoclaim returned exact amount", lambda:
             token_balance(token) == initial_l1 + count * EXIT_WEI, timeout=1200)
        wait(f"{label}: proxy ready", ready)
        return events

    bridge_out("normal", 1)
    pg(f"UPDATE faucet_registry SET metadata=''::bytea WHERE faucet_id='{faucet}'")
    assert row(token)["metadata"] == ""
    bridge_out("empty-preimage-recovery", 2)
    log("PASS: large preimage recovered from origin RPC, emitted byte-for-byte, and backfilled")
    before = event_rows(token)
    (OUT / "events-before-restore.json").write_text(json.dumps(before))

    # No concurrent owner of the proxy sqlite; isolated wallet has its own store.
    compose("stop", "miden-agglayer")
    try:
        pg(f"DELETE FROM faucet_registry WHERE faucet_id='{faucet}'")
        assert row(token) is None
        restored = compose("run", "--rm", "--no-deps", "miden-agglayer",
            "--miden-node=http://miden-node:57291",
            "--miden-store-dir=/var/lib/miden-agglayer-service",
            "--l1-rpc-url=http://anvil:8545", "--network-rpc-url=2=http://anvil-l2b:8545",
            "--reset-miden-store", "--restore", timeout=1200)
        (OUT / "restore.log").write_text(restored)
    finally:
        compose("start", "miden-agglayer")
    wait("proxy ready after missing-row/reset-client restore", ready, timeout=600)
    recovered = row(token)
    assert {key: value for key, value in recovered.items() if key != "metadata"} == identity
    assert bytes.fromhex(recovered["metadata"]) == metadata
    assert event_rows(token) == before, "restore changed historical event bytes/indices/hashes"
    bridge_out("post-restore", 3)
    wait("exact final Miden balance (no duplicate mint)", lambda: wallet_balance(faucet) == 40_000)
    result = {"passed": True, "token": token, "faucet": faucet, "metadata_bytes": len(metadata),
        "metadata_keccak256": digest, "metadata_sha256": hashlib.sha256(metadata).hexdigest(),
        "l1_deposit_gas": int(receipt["gasUsed"], 16), "gas_cap": GAS_CAP,
        "cases": ["real L1 deposit and exact mint", "normal huge-metadata exit and L1 claim",
                  "cleared-preimage live recovery/backfill and L1 claim",
                  "missing-row/reset-client restore and byte-identical retained history",
                  "fresh post-restore exit and L1 claim", "exact final balances"]}
    (OUT / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    log("PASS: HUGE METADATA DEPOSIT / EXIT / LIVE RECOVERY / RESTORE / REPLAY")


def events_at_count(token, count):
    events = event_rows(token)
    assert len(events) <= count, "duplicate event emitted"
    return events if len(events) == count else None


if __name__ == "__main__":
    main()
