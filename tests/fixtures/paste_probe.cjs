// Benign stand-in for an interactive prompt: newlines submit only outside
// bracketed paste. No commands, conversations, clipboard access, or network.
const fs = require('node:fs');
const reportPath = process.argv[2];
let pending = '';
let pasted = false;
let text = '';
let submits = 0;
let pastes = 0;
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
        continue;
      }
      if (!pasted && (char === '\r' || char === '\n')) submits++;
      text += char;
    }
  }
});
process.stdout.write('\x1b[?2004h\x1b[?1000hPASTE_READY\r\n');
