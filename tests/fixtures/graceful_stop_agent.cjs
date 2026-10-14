// Benign agent-like terminal fixture: no network, services, or agent APIs.
const mode = process.argv[2];
const raw = mode !== 'canonical';
let firstInterrupt;
let exiting = false;

if (raw) process.stdin.setRawMode(true);
else process.on('SIGINT', () => {}); // Canonical Ctrl-C is SIGINT, not a byte.
if (mode === 'unresponsive') process.on('SIGTERM', () => {});

function finish(marker) {
  if (exiting) return;
  exiting = true;
  process.stdout.write(marker + '\r\n');
  // Cleanup crosses the old midpoint SIGTERM in the double-Ctrl-C case.
  setTimeout(() => {
    process.stdin.pause();
    if (raw) process.stdin.setRawMode(false);
    let tail = '';
    for (let index = 0; index < 512; index++) {
      tail += `FINAL-TAIL-${String(index).padStart(4, '0')}-` + 'x'.repeat(100) + '\r\n';
    }
    tail += `To resume: helper --resume ${mode}-clean\r\nFINAL-TAIL-END\r\n`;
    // Flush the child's write before exiting; oly must still drain ConPTY/PTY
    // and its journal before reporting a persisted resume hint.
    process.stdout.write(tail, () => process.exit(0));
  }, mode === 'double' ? 2200 : 200);
}

process.stdin.on('data', chunk => {
  if (exiting) return;
  for (const key of chunk) {
    const now = performance.now();
    if (mode === 'double' && key === 0x03) {
      if (firstInterrupt !== undefined && now - firstInterrupt >= 100 && now - firstInterrupt <= 1500) {
        finish('DOUBLE-CONFIRMED');
        return;
      }
      firstInterrupt = now;
      process.stdout.write('INTERRUPT-CONFIRM\r\n');
    } else if (mode === 'eof' && key === 0x04) {
      finish('CTRL-D-ACCEPTED');
      return;
    }
  }
});
process.stdin.on('end', () => {
  if (mode === 'canonical') finish('CANONICAL-EOF-ACCEPTED');
});
process.stdin.resume();
process.stdout.write('STOP-HELPER-READY\r\n');
