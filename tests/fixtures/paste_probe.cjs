// Benign stand-in for an interactive prompt: newlines submit only outside
// bracketed paste. No commands, conversations, clipboard access, or network.
const fs = require('node:fs');
const reportPath = process.argv[2];
const afterPath = reportPath + '.after';
let pending = '';
let pasted = false;
let text = '';
let submits = 0;
let pastes = 0;
let reported = false;
const start = '\x1b[200~';
const end = '\x1b[201~';
process.stdin.setRawMode(true);
process.stdin.setEncoding('utf8');
process.stdin.on('data', chunk => {
  pending += chunk;
  while (pending.length) {
    const marker = pasted ? end : start;
    if (pending.startsWith(marker)) {
      pending = pending.slice(marker.length);
      pasted = !pasted;
      if (pasted) pastes++;
    } else if (marker.startsWith(pending)) {
      return;
    } else {
      const char = pending[0];
      pending = pending.slice(1);
      if (char === '!' && !pasted) {
        // Exercise a live mouse-mode transition without losing VT/raw input.
        process.stdout.write('\x1b[?1000lINPUT_READY\r\n');
        continue;
      }
      if (char === '#' && !pasted) {
        fs.writeFileSync(reportPath, JSON.stringify({ text, submits, pastes }));
        reported = true;
        continue;
      }
      if (!pasted && (char === '\r' || char === '\n')) submits++;
      text += char;
      if (reported) {
        // Everything arriving after the report is diagnostic: the caller
        // reads this file when a later step fails, to see whether the bytes
        // it sent were forwarded here instead of being consumed locally.
        // Note the daemon's staged soft-shutdown inputs (Ctrl-C, Ctrl-C,
        // Ctrl-D) also land here when the session is stopped at
        // teardown, so expect those on a failed run.
        fs.appendFileSync(afterPath, char);
      }
    }
  }
});
process.stdout.write('\x1b[?2004h\x1b[?1000hPASTE_READY\r\n');
