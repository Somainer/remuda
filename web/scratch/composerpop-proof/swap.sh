#!/usr/bin/env bash
# Swap the 7 c-composerpop UI files between fixed (working tree backup) and
# base (3bd07311). Usage: swap.sh base|fixed
set -eu
cd /home/wangruming.nana/Projects/remuda-agents/wt/c-uo0/web
FILES="src/features/session/Composer.tsx src/features/session/ContextUsagePopover.tsx src/features/session/contextUsage.ts src/features/session/contextUsage.module.css src/features/session/popover.module.css src/app/shellNotify.module.css src/styles/tokens.css"
mode=$1
for f in $FILES; do
  cp "scratch/composerpop-proof/$mode/$f" "$f"
done
echo "swapped to $mode"
