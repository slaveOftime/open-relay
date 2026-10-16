// Benign agent-like terminal fixture: no network, services, or agent APIs.
const mode = process.argv[2];
const raw = mode !== 'canonical';
// The daemon's graceful stop writes Ctrl-C, then repeats it after
// min(500ms, grace/4) so the repeat lands inside an application's confirmation
// window; that spacing is pinned by the unit test
// `soft_stop_schedule_second_ctrl_c_lands_inside_double_ctrl_c_window`.
// Delivery, not the schedule, is what varies: a busy host can deliver the
// repeat late, and the PTY can merge bytes the daemon wrote half a second
// apart into a single read, which erases the measured gap. So the floor only
// rejects a press arming itself, the ceiling is the grace these tests pass,
// and a pair merged into one read counts as the repeat.
const DOUBLE_PRESS_FLOOR_MS = 100;
const DOUBLE_PRESS_CEILING_MS = 5000;
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
  let armedThisRead = false;
  for (const key of chunk) {
    const now = performance.now();
    if (mode === 'double' && key === 0x03) {
      const gap = firstInterrupt === undefined ? undefined : now - firstInterrupt;
      const repeated =
        armedThisRead ||
        (gap !== undefined && gap >= DOUBLE_PRESS_FLOOR_MS && gap <= DOUBLE_PRESS_CEILING_MS);
      if (repeated) {
        finish('DOUBLE-CONFIRMED');
        return;
      }
      firstInterrupt = now;
      armedThisRead = true;
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
