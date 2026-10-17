// Deterministic ANSI producer for the log-rendering contract. No shell startup,
// commands, network, or timers: stay alive until the test acknowledges the text.
const fs = require('node:fs');
process.stdin.setRawMode(true);
process.stdin.on('data', chunk => {
  if (chunk.includes(0x78)) process.exit(0); // 'x' acknowledges both log reads.
});
fs.writeSync(1, '\x1b[31mCOLOR_TEST\x1b[0m\r\n');
