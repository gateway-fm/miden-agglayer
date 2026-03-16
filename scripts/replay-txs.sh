#!/bin/sh
# Replays deployment transactions on Anvil to recreate L1 state + events.
# Called during Anvil container startup.

set -e

RPC="http://localhost:8545"
TX_FILE="/state/l1-raw-txs.txt"

echo "==> Funding deployment accounts..."
# Fund with 10,000,000 ETH each (0x84595161401484A000000 = 10M ETH in wei)
for addr in \
    0x5b06837a43bdc3dd9f114558daf4b26ed49842ed \
    0x3fab184622dc19b6109349b94811493bf2a45362 \
    0xc653ecd4ac5153a3700fb13442bcf00a691cca16 \
    0x575c158b45ab2636bcf2b7030e91c9f43c4bd09c \
    0x8943545177806ed17b9f23f0a21ee5948ecaa776 \
    0xe34aaf64b29273b7d567fcfc40544c014eee9970 \
    0x89435451be3fd8df1c67cff6b5bafe98ae10519a; do
    cast rpc anvil_setBalance "$addr" "0x84595161401484A000000" --rpc-url "$RPC" > /dev/null 2>&1
done

echo "==> Replaying deployment transactions..."
COUNT=0
FAILED=0
while IFS= read -r raw_tx; do
    COUNT=$((COUNT + 1))
    result=$(cast rpc eth_sendRawTransaction "$raw_tx" --rpc-url "$RPC" 2>&1) || true
    if echo "$result" | grep -qi "error" 2>/dev/null; then
        FAILED=$((FAILED + 1))
        echo "    tx $COUNT: FAILED"
    fi
done < "$TX_FILE"

echo "==> Replayed $COUNT transactions ($FAILED failed)"

# Mine extra blocks for timestamp advancement
cast rpc anvil_mine 50 --rpc-url "$RPC" > /dev/null 2>&1

BLOCK=$(cast block-number --rpc-url "$RPC" 2>/dev/null || echo "?")
echo "==> Replay complete. Block: $BLOCK"
