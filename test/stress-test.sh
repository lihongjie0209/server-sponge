#!/bin/bash
set -e

echo "============================================"
echo "  Server Sponge — Verification Test Suite"
echo "============================================"

echo ""
echo "[Test 1] Basic startup and PID convergence"
echo "-------------------------------------------"
echo "Starting server-sponge with target=70%, observing convergence..."
server-sponge --target 70 --chunk-size 16 --cooldown 10 --no-psi &
SPONGE_PID=$!
sleep 20

echo ""
echo "Memory status after 20s:"
free -m
echo ""

echo "Stopping sponge..."
kill -SIGTERM $SPONGE_PID 2>/dev/null || true
wait $SPONGE_PID 2>/dev/null || true
sleep 2

echo ""
echo "Memory status after release:"
free -m

echo ""
echo "[Test 2] Panic threshold test"
echo "-----------------------------"
echo "Starting sponge with low panic threshold..."
server-sponge --target 70 --chunk-size 16 --panic-threshold 10 --cooldown 10 --no-psi &
SPONGE_PID=$!
sleep 15

echo "Creating memory pressure with stress-ng..."
stress-ng --vm 1 --vm-bytes 300M --timeout 10s &
STRESS_PID=$!
sleep 12

echo ""
echo "Memory status during/after stress:"
free -m

kill -SIGTERM $SPONGE_PID 2>/dev/null || true
wait $SPONGE_PID 2>/dev/null || true
wait $STRESS_PID 2>/dev/null || true

echo ""
echo "[Test 3] Graceful shutdown"
echo "--------------------------"
server-sponge --target 50 --chunk-size 16 --no-psi &
SPONGE_PID=$!
sleep 10

echo "Sending SIGTERM..."
kill -SIGTERM $SPONGE_PID
wait $SPONGE_PID 2>/dev/null || true
sleep 2

echo "Memory after shutdown:"
free -m

echo ""
echo "============================================"
echo "  All tests completed!"
echo "============================================"
