// Deterministic, network-free terminal program: picker -> main-screen
// transcript, followed by a real fullscreen application. No conversations.
process.stdin.setRawMode(true);
process.stdin.setEncoding('utf8');
process.stdout.write('\x1b[?1049h\x1b[HPICKER_READY');
process.stdin.on('data', chunk => {
  if (chunk.includes('!')) {
    process.stdout.write('\x1b[?1049l\x1b[H\x1b[0J');
    for (let row = 1; row <= 60; row++) {
      process.stdout.write(`TRANSCRIPT_${String(row).padStart(3, '0')}\r\n`);
    }
    process.stdout.write('CHAT_READY');
  } else if (chunk.includes('@')) {
    process.stdout.write('\x1b[?1049h\x1b[H\x1b[0JFULLSCREEN_READY');
  } else if (chunk.includes('#')) {
    process.stdout.write('\x1b[?1049l');
  }
});
