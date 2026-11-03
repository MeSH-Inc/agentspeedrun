#!/usr/bin/env bash
# Exercise cold startup and outbox recovery with bounded CPU load.
# Usage: scripts/startup-smoke.sh <dir containing speedrun and speedrund>
set -euo pipefail

bin=$(cd "${1:?usage: scripts/startup-smoke.sh <bindir>}" && pwd)
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
workers=()
cleanup() {
  for pid in "${workers[@]}"; do kill "$pid" 2>/dev/null || true; done
  for pid in "${workers[@]}"; do wait "$pid" 2>/dev/null || true; done
  rm -rf "$work"
}
trap cleanup EXIT

# Copies keep the daemon next to the CLI, as in a package, without modifying it.
mkdir "$work/bin"
cp "$bin/speedrun" "$work/bin/speedrun"
cp "$bin/speedrund" "$work/bin/speedrund-real"
cat > "$work/bin/speedrund" <<'EOF'
#!/usr/bin/env bash
sleep 2
exec "$(dirname "$0")/speedrund-real" "$@"
EOF
chmod +x "$work/bin/speedrund"

# Two workers are enough to add bounded contention without saturating a host.
for _ in 1 2; do
  yes > /dev/null &
  workers+=("$!")
done

echo 'Cold startup under two CPU workers:'
"$root/scripts/smoke.sh" "$bin"
echo 'Two-second delayed startup under two CPU workers:'
SPEEDRUN_SMOKE_EXPECT_SPOOL=1 "$root/scripts/smoke.sh" "$work/bin"
